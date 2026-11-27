fn main() {
    println!("cargo:rerun-if-changed=dataduck.rc");
    println!("cargo:rerun-if-changed=assets/dataduck.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("dataduck.rc", embed_resource::NONE)
            .manifest_required()
            .expect("compile the DataDuck Windows application icon");
    }
}
