fn main() {
    embed_resource::compile("res/mono.rc", embed_resource::NONE).manifest_optional().unwrap();
    println!("cargo:rerun-if-changed=res/mono.rc");
    println!("cargo:rerun-if-changed=res/mono.ico");
    println!("cargo:rerun-if-changed=ui");
}
