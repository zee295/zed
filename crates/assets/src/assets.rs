// This crate was essentially pulled out verbatim from main `zed` crate to avoid having to run RustEmbed macro whenever zed has to be rebuilt. It saves a second or two on an incremental build.

#[cfg(not(target_family = "wasm"))]
use anyhow::Context as _;
use gpui::{App, AssetSource, Result, SharedString};
#[cfg(not(target_family = "wasm"))]
use rust_embed::RustEmbed;
#[cfg(target_family = "wasm")]
use std::{borrow::Cow, collections::BTreeMap, sync::OnceLock};

#[cfg(not(target_family = "wasm"))]
#[derive(RustEmbed)]
#[folder = "../../assets"]
#[include = "fonts/**/*"]
#[include = "icons/**/*"]
#[include = "images/**/*"]
#[include = "themes/**/*"]
#[exclude = "themes/src/*"]
#[include = "sounds/**/*"]
#[include = "prompts/**/*"]
#[include = "*.md"]
#[exclude = "*.DS_Store"]
pub struct Assets;

#[cfg(target_family = "wasm")]
pub struct Assets;

#[cfg(target_family = "wasm")]
static WEB_ASSETS: OnceLock<BTreeMap<String, Vec<u8>>> = OnceLock::new();

#[cfg(target_family = "wasm")]
pub fn install_web_assets(assets: BTreeMap<String, Vec<u8>>) -> Result<()> {
    WEB_ASSETS
        .set(assets)
        .map_err(|_| anyhow::anyhow!("web assets were already installed"))
}

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<std::borrow::Cow<'static, [u8]>>> {
        #[cfg(not(target_family = "wasm"))]
        {
            Self::get(path)
                .map(|file| Some(file.data))
                .with_context(|| format!("loading asset at path {path:?}"))
        }
        #[cfg(target_family = "wasm")]
        {
            Ok(WEB_ASSETS
                .get()
                .and_then(|assets| assets.get(path))
                .map(|bytes| Cow::Borrowed(bytes.as_slice())))
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        #[cfg(not(target_family = "wasm"))]
        {
            Ok(Self::iter()
                .filter_map(|asset_path| asset_path.starts_with(path).then(|| asset_path.into()))
                .collect())
        }
        #[cfg(target_family = "wasm")]
        {
            Ok(WEB_ASSETS
                .get()
                .into_iter()
                .flat_map(BTreeMap::keys)
                .filter(|asset_path| asset_path.starts_with(path))
                .cloned()
                .map(SharedString::from)
                .collect())
        }
    }
}

impl Assets {
    /// Populate the [`TextSystem`] of the given [`AppContext`] with all `.ttf` fonts in the `fonts` directory.
    pub fn load_fonts(&self, cx: &App) -> anyhow::Result<()> {
        let font_paths = self.list("fonts")?;
        let mut embedded_fonts = Vec::new();
        for font_path in font_paths {
            if font_path.ends_with(".ttf") {
                let font_bytes = cx
                    .asset_source()
                    .load(&font_path)?
                    .expect("Assets should never return None");
                embedded_fonts.push(font_bytes);
            }
        }

        cx.text_system().add_fonts(embedded_fonts)
    }

    pub fn load_test_fonts(&self, cx: &App) {
        cx.text_system()
            .add_fonts(vec![
                self.load("fonts/lilex/Lilex-Regular.ttf").unwrap().unwrap(),
            ])
            .unwrap()
    }
}
