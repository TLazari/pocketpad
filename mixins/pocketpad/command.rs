// /pocketpad (ou /celular): põe o endereço no chat e mostra ou esconde o QR do canto.
fn pocketpad_command(game: &mut crate::game::Game<'_>, command: &str, ran: &mut bool) {
    use crate::game::pocketpad as m;
    if !game.native_on(m::ID) || !matches!(command.split(' ').next(), Some("pocketpad" | "celular")) {
        return;
    }
    *ran = true;
    m::start();
    let s = m::status();
    let shown = m::toggle_qr();
    // Os textos no idioma do jogo (inglês nos que o mod não traduz).
    let lang = game.settings.language.clone();
    let (error, starting, qr_on, qr_off) = match lang.get(..2) {
        Some("pt") => ("erro", "ligando o servidor...", "QR no canto da tela (/pocketpad de novo esconde)", "QR escondido (/pocketpad mostra)"),
        Some("es") => ("error", "iniciando el servidor...", "QR en la esquina de la pantalla (/pocketpad otra vez lo oculta)", "QR oculto (/pocketpad lo muestra)"),
        _ => ("error", "starting the server...", "QR in the corner of the screen (/pocketpad again hides it)", "QR hidden (/pocketpad shows it)"),
    };
    match (&s.error, s.url.is_empty()) {
        (Some(e), _) => game.native_chat(&format!("PocketPad: {error}, {e}")),
        (None, true) => game.native_chat(&format!("PocketPad: {starting}")),
        (None, false) => game.native_chat(&format!("PocketPad: {}", s.url)),
    }
    game.native_chat(if shown { qr_on } else { qr_off });
}
