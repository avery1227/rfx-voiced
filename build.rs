// Mumble.proto is vendored verbatim from the FiveM source tree
// (code/components/voip-server-mumble/src/Mumble.proto) so the wire format we
// speak is by definition the one the server implements.
//
// protoc comes from protoc-bin-vendored rather than the system, so a build
// needs nothing installed - which matters because this is built both on a
// Windows dev box and in a slim Linux container.

fn main() {
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc");
    std::env::set_var("PROTOC", protoc);

    prost_build::compile_protos(&["proto/Mumble.proto"], &["proto"]).expect("compile Mumble.proto");

    println!("cargo:rerun-if-changed=proto/Mumble.proto");
}
