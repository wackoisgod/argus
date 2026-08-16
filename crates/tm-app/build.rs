fn main() {
    // Delay-load DLLs that are not needed before the first frame renders, so
    // process init maps fewer modules (same trick TaskSlinger uses via
    // .didat). icuuc is the heavyweight; version.dll is only touched by the
    // enrichment pool seconds after launch; shell32/comctl32 only serve
    // dialogs. The DirectX/DirectWrite family stays eager — it loads before
    // the first frame regardless.
    for dll in ["icuuc.dll", "version.dll", "shell32.dll"] {
        println!("cargo:rustc-link-arg=/DELAYLOAD:{dll}");
    }
    println!("cargo:rustc-link-arg=delayimp.lib");
}
