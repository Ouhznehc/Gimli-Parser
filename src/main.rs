use fallible_iterator::FallibleIterator;
use gimli::Reader as _;
use object::{Object, ObjectSection};
use std::{borrow, env, error, fs};

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

// The reader type that will be stored in `Dwarf` and `DwarfPackage`.
// If you don't need relocations, you can use `gimli::EndianSlice` directly.
type Reader<'data> =
    gimli::RelocateReader<gimli::EndianSlice<'data, gimli::RunTimeEndian>, &'data RelocationMap>;

fn main() {
    let mut args = env::args();
    if args.len() != 2 {
        println!("Usage: {} <file>", args.next().unwrap());
        return;
    }
    args.next().unwrap();
    let path = args.next().unwrap();

    let file = fs::File::open(path).unwrap();
    let mmap = unsafe { memmap2::Mmap::map(&file).unwrap() };
    let object = object::File::parse(&*mmap).unwrap();
    let endian = if object.is_little_endian() {
        gimli::RunTimeEndian::Little
    } else {
        gimli::RunTimeEndian::Big
    };

    dump_file(&object, endian).unwrap();
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

        println!("{}<{}> {}", depth, entry.offset().0, entry.tag());

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
    let mut attrs = entry.attrs();
    while let Some(attr) = attrs.next()? {
        match attr.name() {
            gimli::DW_AT_name | gimli::DW_AT_linkage_name => {
                println!(
                    "   {}: {:?}",
                    attr.name(),
                    dw_at_name_handler(&unit, &attr)?
                );
            }
            gimli::DW_AT_type => {
                println!("   {}: {:?}", attr.name(), dw_at_type_handler(&attr)?);
            }
            _ => {
                // println!("   {}: Unparsed Attribute", attr.name());
                continue;
            }
        }
    }
    Ok(())
}


/// Handler for DW_TAG_variable, which is a local variable.
/// we are interested in the name, type, and location(stack offset) of the variable.
fn dw_tag_variable_handler<'a>(
    unit: &gimli::UnitRef<Reader<'a>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'a>>,
) -> Result<(), gimli::Error> {
    let mut attrs = entry.attrs();
    while let Some(attr) = attrs.next()? {
        match attr.name() {
            gimli::DW_AT_name => {
                println!(
                    "   {}: {:?}",
                    attr.name(),
                    dw_at_name_handler(&unit, &attr)?
                );
            }
            gimli::DW_AT_type => {
                println!("   {}: {:?}", attr.name(), dw_at_type_handler(&attr)?);
            }
            gimli::DW_AT_location => {
                println!("   {}: {:?}", attr.name(), dw_at_location_handler(&unit, &attr)?);
            }
            _ => {
                // println!("   {}: Unparsed Attribute", attr.name());
                continue;
            }
        }
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
) -> Result<(), gimli::Error> {
    let expression = attr.exprloc_value().unwrap();
    let mut eval = expression.evaluation(unit.encoding());
    let mut result = eval.evaluate().unwrap();
    loop {
        match result {
            gimli::EvaluationResult::Complete => {
                let value = eval.result();
                print!("   {}: {:?}", attr.name(), value);
                break;
            }
            gimli::EvaluationResult::RequiresFrameBase => {
                result = eval.resume_with_frame_base(0).unwrap();
            }
            _ => {
                print!("   {}: {:?}", attr.name(), result);
                break;
            }
        }
    }
    Ok(())
}
