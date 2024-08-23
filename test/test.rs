fn main() {
    let main_local_variable = 1;
    {
        let main_local_variable = 2;
        println!("Hello World nested with main_local_variable = {}", main_local_variable);
    }
    println!("Hello World with main_local_variable = {}", main_local_variable);
}