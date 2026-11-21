fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }

    // winres вшивает ресурс во все бинарники пакета. Для вспомогательных
    // (preview / probe) оставляем asInvoker, для основного — admin.
    // Cargo не даёт отдельный build.rs на bin, поэтому ориентируемся на
    // переменную окружения AJAZZ_BIN / имя профиля через OUT_DIR эвристику.
    let manifest = match std::env::var("AJAZZ_MANIFEST").ok().as_deref() {
        Some("user") => "app-user.manifest",
        _ => "app.manifest",
    };

    let mut res = winres::WindowsResource::new();
    res.set_manifest_file(manifest);
    if let Err(error) = res.compile() {
        println!("cargo:warning=не удалось встроить манифест ({manifest}): {error}");
    }
}
