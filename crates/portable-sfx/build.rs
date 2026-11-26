fn main() {
    println!("cargo:rerun-if-changed=../../apps/rstream/windows/rstream.rc");
    println!("cargo:rerun-if-changed=../../apps/rstream/windows/rstream.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("../../apps/rstream/windows/rstream.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("compile RStream Windows icon resource");
    }
}
