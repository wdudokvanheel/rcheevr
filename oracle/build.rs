use std::path::PathBuf;

fn main() {
    let root: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let vendor = root.join("vendor/rcheevos");
    let include = vendor.join("include");
    let src = vendor.join("src");
    let rcheevos_dir = src.join("rcheevos");

    if !vendor.exists() {
        panic!(
            "rcheevos submodule missing at {}. Run `git submodule update --init`.",
            vendor.display()
        );
    }

    let sources: &[&str] = &[
        "alloc.c",
        "condition.c",
        "condset.c",
        "consoleinfo.c",
        "format.c",
        "lboard.c",
        "memref.c",
        "operand.c",
        "rc_validate.c",
        "richpresence.c",
        "runtime.c",
        "runtime_progress.c",
        "trigger.c",
        "value.c",
    ];

    let mut build = cc::Build::new();
    build
        .include(&include)
        .include(&rcheevos_dir)
        .file(src.join("rc_compat.c"))
        .file(src.join("rc_util.c"))
        .warnings(false)
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-unused-function")
        .flag_if_supported("-Wno-unused-but-set-variable")
        .flag_if_supported("-std=c99");

    for s in sources {
        build.file(rcheevos_dir.join(s));
    }

    build.compile("rcheevos");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", vendor.display());
}
