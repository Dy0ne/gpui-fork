use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

/// Assets compiled into the binary. Icons are from Lucide
/// (`assets/icons/LICENSE-lucide`).
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        let bytes: &'static [u8] = match path {
            "icons/minus.svg" => include_bytes!("../assets/icons/minus.svg"),
            "icons/plus.svg" => include_bytes!("../assets/icons/plus.svg"),
            "icons/shuffle.svg" => include_bytes!("../assets/icons/shuffle.svg"),
            _ => return Ok(None),
        };
        Ok(Some(Cow::Borrowed(bytes)))
    }

    fn list(&self, _path: &str) -> Result<Vec<SharedString>> {
        Ok(vec![
            "icons/minus.svg".into(),
            "icons/plus.svg".into(),
            "icons/shuffle.svg".into(),
        ])
    }
}
