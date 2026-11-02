fn main() {
    println!("cargo:rerun-if-changed=assets/pegline.manifest");
    println!("cargo:rerun-if-changed=assets/pegline.ico");
    println!("cargo:rerun-if-changed=assets/pegline.rc");

    // The manifest is required: it declares Per-Monitor V2 awareness and the
    // supported OS list, which the overlay depends on for correct DPI math.
    if let Err(err) =
        embed_resource::compile("assets/pegline.rc", embed_resource::NONE).manifest_required()
    {
        panic!("failed to embed Windows resources: {err}");
    }
}
