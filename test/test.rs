#[derive(Debug)] 
struct MyFatherStruct {
    field: i32,
    nest: MyChildStruct,
}

#[derive(Debug)]
struct MyChildStruct {
    field: i32,
}

fn main() {
    let father = MyFatherStruct {
        field: 1,
        nest: MyChildStruct {
            field: 2,
        },
    };
    let main_local_variable = 1;
    {
        let main_local_variable = 2;
        println!("Hello World nested with main_local_variable = {}", main_local_variable);
    }
    println!("Hello World with main_local_variable = {}", main_local_variable);
    println!("Hello World with father {:?}", father);
}