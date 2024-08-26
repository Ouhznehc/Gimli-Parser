use fallible_iterator::FallibleIterator;
use gimli::Reader as _;
use lazy_static::lazy_static;
use object::{Object, ObjectSection};
use serde_json::to_writer_pretty;
use std::collections::HashMap;
use std::sync::RwLock;
use std::{borrow, env, error, fs};

lazy_static! {
    // The map that stores the subprogram data.
    static ref SUBPROGRAM_MAP: RwLock<HashMap<String, Subprogram>> = RwLock::new(HashMap::new());
    static ref CURRENT_SUBPROGRAM: RwLock<Option<String>> = RwLock::new(None);
}

// This is a simple wrapper around `object::read::RelocationMap` that implements
// `gimli::read::Relocate` for use with `gimli::RelocateReader`.
// You only need this if you are parsing relocatable object files.
#[derive(Debug, Default)]
struct RelocationMap(object::read::RelocationMap);

impl<'a> gimli::read::Relocate for &'a RelocationMap {
    fn relocate_address(&self, offset: usize, value: u64) -> gimli::Result<u64> {
        Ok(self.0.relocate(offset as u64, value))
    }

    fn relocate_offset(&self, offset: usize, value: usize) -> gimli::Result<usize> {
        <usize as gimli::ReaderOffset>::from_u64(self.0.relocate(offset as u64, value as u64))
    }
}

// The section data that will be stored in `DwarfSections` and `DwarfPackageSections`.
#[derive(Default)]
struct Section<'data> {
    data: borrow::Cow<'data, [u8]>,
    relocations: RelocationMap,
}

// The struct that represents a local variable in the stack.
// var_type is a usize that stands for a DW_TAG_type node.
// location is a stack offset and is None if the location expression is not `RequiredFrameBase`.
#[derive(Debug, serde::Serialize)]
struct Variable {
    name: String,
    var_type: usize,
    location: Option<i64>,
}

// The struct that represents a function or method.
// The linkage_name is used as the key in the subprogram map, and it stands for the function name in elf file.
#[derive(Debug, serde::Serialize)]
struct Subprogram {
    name: String,
    linkage_name: String,
    ret_type: usize,
    variables: Vec<Variable>,
}

// The reader type that will be stored in `Dwarf` and `DwarfPackage`.
// If you don't need relocations, you can use `gimli::EndianSlice` directly.
type Reader<'data> =
    gimli::RelocateReader<gimli::EndianSlice<'data, gimli::RunTimeEndian>, &'data RelocationMap>;

fn main() {
    let mut args = env::args();
    if args.len() != 4 {
        println!(
            "Usage: {} <file> <subprogram.out> <type.out>",
            args.next().unwrap()
        );
        return;
    }
    args.next().unwrap();
    let path = args.next().unwrap();
    // The output file for the subprogram data, which is a JSON file.
    // The JSON file contains the name, linkage name, return type, and local variables of each function.
    let subprogram_out = args.next().unwrap();
    // The output file for the type data, which is a JSON file.
    let _type_out = args.next().unwrap();

    let file = fs::File::open(path).unwrap();
    let mmap = unsafe { memmap2::Mmap::map(&file).unwrap() };
    let object = object::File::parse(&*mmap).unwrap();
    let endian = if object.is_little_endian() {
        gimli::RunTimeEndian::Little
    } else {
        gimli::RunTimeEndian::Big
    };

    dump_file(&object, endian).unwrap();

    let map = SUBPROGRAM_MAP.read().unwrap();
    let file = fs::File::create(subprogram_out).expect("Unable to create file");
    to_writer_pretty(file, &*map).expect("Unable to write data");
    println!("Data successfully written to the output file.");
}

/// Get the DWARF information from the object file.
fn dump_file(
    object: &object::File,
    endian: gimli::RunTimeEndian,
) -> Result<(), Box<dyn error::Error>> {
    // Load a `Section` that may own its data.
    fn load_section<'data>(
        object: &object::File<'data>,
        name: &str,
    ) -> Result<Section<'data>, Box<dyn error::Error>> {
        Ok(match object.section_by_name(name) {
            Some(section) => Section {
                data: section.uncompressed_data()?,
                relocations: section.relocation_map().map(RelocationMap)?,
            },
            None => Default::default(),
        })
    }

    // Borrow a `Section` to create a `Reader`.
    fn borrow_section<'data>(
        section: &'data Section<'data>,
        endian: gimli::RunTimeEndian,
    ) -> Reader<'data> {
        let slice = gimli::EndianSlice::new(borrow::Cow::as_ref(&section.data), endian);
        gimli::RelocateReader::new(slice, &section.relocations)
    }

    // Load all of the sections.
    let dwarf_sections = gimli::DwarfSections::load(|id| load_section(object, id.name()))?;

    // Create `Reader`s for all of the sections and do preliminary parsing.
    // Alternatively, we could have used `Dwarf::load` with an owned type such as `EndianRcSlice`.
    let dwarf = dwarf_sections.borrow(|section| borrow_section(section, endian));

    // Iterate over the compilation units.
    // We only need to iterate over the compilation units in the `.debug_info` section.
    let mut iter = dwarf.units();
    let debug_info_header = iter
        .find(|header| Ok(header.offset().as_debug_info_offset().unwrap().0 == 0))
        .expect("No .debug_info header found")
        .unwrap();

    let unit = dwarf.unit(debug_info_header)?;
    let unit_ref = unit.unit_ref(&dwarf);
    dump_unit(unit_ref)?;

    Ok(())
}

/// Iterate over the Debugging Information Entries (DIEs) in the unit.
fn dump_unit(unit: gimli::UnitRef<Reader>) -> Result<(), gimli::Error> {
    // Iterate over the Debugging Information Entries (DIEs) in the unit.
    let mut depth = 0;
    let mut entries = unit.entries();
    while let Some((delta_depth, entry)) = entries.next_dfs()? {
        depth += delta_depth;

        println!("<{}><{}> {}", depth, entry.offset().0, entry.tag());

        match entry.tag() {
            gimli::DW_TAG_subprogram => dw_tag_subprogram_handler(&unit, &entry)?,
            gimli::DW_TAG_variable => dw_tag_variable_handler(&unit, &entry)?,
            _ => dw_tag_default_handler(&unit, &entry)?,
        }
    }
    Ok(())
}

/// Handler for DW_TAG_subprogram, which is a function or method.
/// we are interested in the name, linkage name, and return type of the function.
fn dw_tag_subprogram_handler<'a>(
    unit: &gimli::UnitRef<Reader<'a>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'a>>,
) -> Result<(), gimli::Error> {
    let mut name = String::new();
    let mut linkage_name = String::new();
    let mut ret_type = 0;

    let mut attrs = entry.attrs();
    while let Some(attr) = attrs.next()? {
        match attr.name() {
            gimli::DW_AT_name => {
                name = dw_at_name_handler(&unit, &attr)?;
                println!("   {}: {:?}", attr.name(), name);
            }
            gimli::DW_AT_linkage_name => {
                linkage_name = dw_at_name_handler(&unit, &attr)?;
                println!("   {}: {:?}", attr.name(), linkage_name);
            }
            gimli::DW_AT_type => {
                println!("   {}: {:?}", attr.name(), dw_at_type_handler(&attr)?);
                ret_type = dw_at_type_handler(&attr)?;
            }
            _ => {
                // println!("   {}: Unparsed Attribute", attr.name());
                continue;
            }
        }
    }

    // Insert the subprogram data into the map.
    let mut map = SUBPROGRAM_MAP.write().unwrap();
    map.insert(
        linkage_name.clone(),
        Subprogram {
            name,
            linkage_name: linkage_name.clone(),
            ret_type,
            variables: Vec::new(),
        },
    );

    // Update the current subprogram.
    let mut current_subprogram = CURRENT_SUBPROGRAM.write().unwrap();
    *current_subprogram = Some(linkage_name.clone());

    Ok(())
}

/// Handler for DW_TAG_variable, which is a local variable.
/// we are interested in the name, type, and location(stack offset) of the variable.
fn dw_tag_variable_handler<'a>(
    unit: &gimli::UnitRef<Reader<'a>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'a>>,
) -> Result<(), gimli::Error> {
    let mut name = String::new();
    let mut var_type = 0;
    let mut location = None;

    let mut attrs = entry.attrs();
    while let Some(attr) = attrs.next()? {
        match attr.name() {
            gimli::DW_AT_name => {
                name = dw_at_name_handler(&unit, &attr)?;
                println!("   {}: {:?}", attr.name(), name);
            }
            gimli::DW_AT_type => {
                var_type = dw_at_type_handler(&attr)?;
                println!("   {}: {:?}", attr.name(), var_type);
            }
            gimli::DW_AT_location => {
                location = dw_at_location_handler(&unit, &attr)?;
            }
            _ => {
                // println!("   {}: Unparsed Attribute", attr.name());
                continue;
            }
        }
    }

    // The current subprogram is the key in the subprogram map.
    // If the current subprogram is None, which stand for a global variable, we just ignore it.
    let linkage_name = {
        let current_subprogram = CURRENT_SUBPROGRAM.read().unwrap();
        match &*current_subprogram {
            Some(name) => name.clone(),
            None => {
                return Ok(());
            }
        }
    };

    let mut map = SUBPROGRAM_MAP.write().unwrap();
    if let Some(subprogram) = map.get_mut(&linkage_name) {
        subprogram.variables.push(Variable {
            name,
            var_type,
            location,
        });
    }

    Ok(())
}

/// Handler for other DW_TAG_*, which is currently not parsed.
/// we just print all the attributes.
fn dw_tag_default_handler<'a>(
    unit: &gimli::UnitRef<Reader<'a>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'a>>,
) -> Result<(), gimli::Error> {
    let mut attrs = entry.attrs();
    while let Some(attr) = attrs.next()? {
        println!(
            "   {}: {:?}",
            attr.name(),
            dw_at_name_handler(&unit, &attr)?
        );
    }
    Ok(())
}

/// Handler for DW_AT_name, which is a string attribute.
/// we convert the attribute value from a DebugStrRef(offset) to a string.
fn dw_at_name_handler<'a>(
    unit: &gimli::UnitRef<Reader<'a>>,
    attr: &gimli::Attribute<Reader<'a>>,
) -> Result<String, gimli::Error> {
    match unit.attr_string(attr.value()) {
        Ok(string) => Ok(string.to_string_lossy()?.to_string()),
        Err(_) => Ok(format!("{:?}", attr.value())),
    }
}

/// Handler for DW_AT_type, which is a reference to another DW_TAG_type.
/// we convert the attribute value from a UnitRef(offset) to a usize, which stands for a DW_TAG_type node.
fn dw_at_type_handler<'a>(attr: &gimli::Attribute<Reader<'a>>) -> Result<usize, gimli::Error> {
    if let gimli::AttributeValue::UnitRef(offset) = attr.value() {
        Ok(offset.0)
    } else {
        Err(gimli::Error::UnsupportedOffset)
    }
}

/// Handler for DW_AT_location, which is a location expression.
/// we evaluate the expression and print the result.
fn dw_at_location_handler(
    unit: &gimli::Unit<Reader>,
    attr: &gimli::Attribute<Reader>,
) -> Result<Option<i64>, gimli::Error> {
    let expression = attr.exprloc_value().unwrap();
    let mut eval = expression.evaluation(unit.encoding());
    let mut result = eval.evaluate().unwrap();
    loop {
        match result {
            // When calculation is complete, print the result.
            gimli::EvaluationResult::Complete => {
                let value = eval
                    .value_result()
                    .unwrap()
                    .convert(gimli::ValueType::I64, 0xFFFFFFFFFFFFFFFF)
                    .unwrap();
                match value {
                    gimli::Value::I64(val) => {
                        println!("   {}: {:?}", attr.name(), val);
                        return Ok(Some(val));
                    }
                    _ => {
                        println!("   {}: {:?}", attr.name(), value);
                        return Ok(None);
                    }
                }
            }
            // We currently only care about the RequiresFrameBase Expression.
            // Set the frame base to 0 to calculate the offset.
            gimli::EvaluationResult::RequiresFrameBase => {
                result = eval.resume_with_frame_base(0).unwrap();
            }
            // Unparsed Expression, print the result and break.
            _ => {
                println!("   {}: Unparsed Expression: {:?}", attr.name(), result);
                return Ok(None);
            }
        }
    }
}
