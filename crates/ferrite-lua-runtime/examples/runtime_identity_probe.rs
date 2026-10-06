fn main() {
    println!("IDENTITY {:?}", ferrite_lua_runtime::runtime_identity());
    let vm = ferrite_lua_runtime::new_vm();
    let value: i64 = vm.load("local n=0; for i=1,1000 do n=n+i end; return n").eval().unwrap();
    assert_eq!(value, 500500);
    println!("VM_SUM {value}");
}
