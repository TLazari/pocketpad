// A linha do mod em Opções > Mods.
fn pocketpad_listed(mods: &mut Vec<crate::game::native_hooks::NativeMod>) {
    use crate::game::pocketpad as m;
    mods.push(crate::game::native_hooks::NativeMod {
        id: m::ID.into(),
        title: format!("{} {}", m::NAME, m::VERSION),
        description: m::DESCRIPTION.into(),
        keys: Vec::new(),
    });
}
