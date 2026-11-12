use std::path::Path;

fn main() {
    let dir = Path::new("quickjs");
    let version = std::fs::read_to_string(dir.join("VERSION")).unwrap();
    let mut build = cc::Build::new();
    build
        .files(
            [
                "quickjs.c",
                "dtoa.c",
                "libregexp.c",
                "libunicode.c",
                "cutils.c",
            ]
            .iter()
            .map(|f| dir.join(f)),
        )
        .file("src/static_wrappers.c")
        .include(dir)
        .define("_GNU_SOURCE", None)
        .define("CONFIG_VERSION", format!("\"{}\"", version.trim()).as_str())
        .flag_if_supported("-fwrapv")
        .flag_if_supported("-Wno-array-bounds")
        .flag_if_supported("-Wno-format-truncation")
        .flag_if_supported("-Wno-infinite-recursion")
        .warnings(false);
    // Optimized in debug builds too, like SpiderMonkey: at -O0 a promise job takes twice the
    // instructions. `CFLAGS=-O0` still applies after this, for debugging QuickJS itself.
    if std::env::var("OPT_LEVEL").as_deref() == Ok("0") {
        build.opt_level(2);
    }
    if std::env::var_os("CARGO_FEATURE_DUMP_LEAKS").is_some() {
        build.define("DUMP_LEAKS", "1");
    }
    // Servo can't name the feature, which SpiderMonkey's mozjs doesn't have, so its mach sets the
    // variable for builds with devtools.
    println!("cargo:rerun-if-env-changed=SERVO_QUICKJS_DEBUGGER");
    if std::env::var_os("CARGO_FEATURE_DEBUGGER").is_some()
        || std::env::var("SERVO_QUICKJS_DEBUGGER").as_deref() == Ok("1")
    {
        build.define("CONFIG_DEBUGGER", None);
        // `DEP_QUICKJS_DEBUGGER` for mozjs's build script.
        println!("cargo:debugger=1");
    }
    build.compile("quickjs");
    println!("cargo:rerun-if-changed=quickjs");
    println!("cargo:rerun-if-changed=src/static_wrappers.c");
}
