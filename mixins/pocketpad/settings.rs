// A página do PocketPad em Opções > Mods: conectar o celular (o QR grande no
// meio da tela), o QR no canto ao entrar, e o endereço e o código da sala.
fn pocketpad_settings(game: &mut crate::game::Game<'_>, mod_id: &str, buttons: &mut Vec<String>) {
    use crate::game::pocketpad as m;
    if mod_id != m::ID {
        return;
    }
    m::start();
    let lang = game.settings.language.clone();
    let t = |k| m::text(&lang, k);
    let auto = m::qr_auto(|| game.native_setting(m::ID, "qr_auto"));
    let s = m::status();
    buttons.push(if s.connected { t("connected") } else { t("connect") }.to_owned());
    buttons.push(format!("{}: {}", t("auto"), if auto { t("on") } else { t("off") }));
    buttons.push(match &s.error {
        Some(e) => format!("{}: {e}", t("error")),
        None if s.url.is_empty() => t("starting").to_owned(),
        None => format!(
            "{} {} · {} {}",
            t("address"),
            m::short_address(&s.url),
            t("code"),
            m::room_code()
        ),
    });
}
