// Mumble.proto is vendored verbatim from the FiveM source tree
// (code/components/voip-server-mumble/src/Mumble.proto) so the wire format we
// speak is by definition the one the server implements.
//
// protoc comes from protoc-bin-vendored rather than the system, so a build
// needs nothing installed - which matters because this is built both on a
// Windows dev box and in a slim Linux container.

fn main() {
    // Stamp the build so a running node can say what it is.
    //
    // Without this the only way to tell whether a deploy took was to infer it
    // from behaviour - and on Pelican, where a restart runs the binary already
    // on disk and only a reinstall downloads, that is a guess you make wrong.
    //
    // GITHUB_REF_NAME is the tag in CI. `git describe` covers a local build.
    // Neither is fatal: an unknown version is worth less than a failed build.
    let version = std::env::var("GITHUB_REF_NAME")
        .ok()
        .filter(|v| v.starts_with('v'))
        .or_else(|| {
            std::process::Command::new("git")
                .args(["describe", "--tags", "--always", "--dirty"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| format!("v{}", env!("CARGO_PKG_VERSION")));

    println!("cargo:rustc-env=VOICED_VERSION={version}");
    println!("cargo:rerun-if-env-changed=GITHUB_REF_NAME");

    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc");
    std::env::set_var("PROTOC", protoc);

    prost_build::compile_protos(&["proto/Mumble.proto"], &["proto"]).expect("compile Mumble.proto");

    println!("cargo:rerun-if-changed=proto/Mumble.proto");
}
