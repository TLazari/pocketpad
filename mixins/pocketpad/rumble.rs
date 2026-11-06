// A vibração que o jogo pede ao controle (dano, bloco quebrado, explosão,
// totem): o celular vibra igual. Roda antes de o jogo olhar se há controle
// físico ou se a vibração está ligada nas opções.
fn pocketpad_rumble(rumble: horizon_client::gamepad::Rumble) {
    if let Some(pulse) = rumble.pulse() {
        crate::game::pocketpad::rumble(pulse.strong, pulse.weak, pulse.millis);
    }
}
