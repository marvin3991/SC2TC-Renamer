use crate::native;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, io::Read, path::Path};
use zhconv::{Variant, ZhConverter};

pub const ENGINE_VERSION: &str = "0.4.2";
pub const MODE: &str = "zh-Hant.json";
pub const EMBEDDED_SOURCE_SHA256: &str =
    "5cb0019b32bb39ec5c6e662029f90bd166f7a844efb3bc877f9be41fdd511bf2";
pub const EMBEDDED_SOURCE: &[u8] = include_bytes!("../vendor/mediawiki/ZhConversion.php");
pub const MAX_SOURCE_BYTES: u64 = 4 * 1024 * 1024;
pub const MAX_CONFIG_BYTES: u64 = 8 * 1024 * 1024;
const MAX_TABLE_RULES: usize = 100_000;
const MAX_RULE_BYTES: usize = 4096;
const TABLE_NAMES: [&str; 5] = [
    "ZH_TO_HANT",
    "ZH_TO_HANS",
    "ZH_TO_TW",
    "ZH_TO_HK",
    "ZH_TO_CN",
];
pub type MediaWikiTables = BTreeMap<String, BTreeMap<String, String>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Mode {
    ZhHant,
    ZhTw,
}
impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Self::ZhHant => "zh-Hant",
            Self::ZhTw => "zh-TW",
        }
    }
    pub fn config(self) -> &'static str {
        match self {
            Self::ZhHant => "zh-Hant.json",
            Self::ZhTw => "zh-TW.json",
        }
    }
    pub fn description(self) -> &'static str {
        match self {
            Self::ZhHant => "一般繁體",
            Self::ZhTw => "台灣繁體與常用詞",
        }
    }
    fn variant(self) -> Variant {
        match self {
            Self::ZhHant => Variant::ZhHant,
            Self::ZhTw => Variant::ZhTW,
        }
    }
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "zh-Hant" | "zh-Hant.json" => Ok(Self::ZhHant),
            "zh-TW" | "zh-TW.json" => Ok(Self::ZhTw),
            _ => bail!("名稱備份的轉換模式不支援。"),
        }
    }
}

enum Backend {
    Embedded(&'static ZhConverter),
    External(Box<ZhConverter>),
}
pub struct Converter {
    backend: Backend,
    pub dictionary_version: String,
    pub dictionary_hash: String,
}
impl Converter {
    pub fn new() -> Result<Self> {
        Self::with_mode(Mode::ZhHant)
    }
    pub fn with_mode(mode: Mode) -> Result<Self> {
        if let Some(bundle) = crate::updater::Store::standard()?.active()? {
            let mut converter = Self::from_config(&bundle.directory.join(mode.config()))?;
            converter.dictionary_version = bundle.version;
            converter.dictionary_hash = bundle.digest;
            return Ok(converter);
        }
        ensure!(
            source_hash(EMBEDDED_SOURCE) == EMBEDDED_SOURCE_SHA256,
            "內附 MediaWiki 來源與建置版本不一致"
        );
        Ok(Self {
            backend: Backend::Embedded(zhconv::get_builtin_converter(mode.variant())),
            dictionary_version: ENGINE_VERSION.to_owned(),
            dictionary_hash: crate::updater::EMBEDDED_CRATE_SHA256.to_owned(),
        })
    }
    pub fn from_config(path: &Path) -> Result<Self> {
        let info = native::metadata(path)?;
        ensure!(
            !info.link && info.identity.size.is_some_and(|n| n <= MAX_CONFIG_BYTES),
            "MediaWiki 模式設定不是有效的實體檔案"
        );
        let mut bytes = Vec::new();
        fs::File::open(path)?
            .take(MAX_CONFIG_BYTES + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= MAX_CONFIG_BYTES,
            "模式設定讀取超出上限"
        );
        let config: RulesConfig = serde_json::from_slice(&bytes)?;
        ensure!(config.schema == 1, "MediaWiki 模式設定版本不支援");
        ensure!(
            config.engine_version == ENGINE_VERSION,
            "字典設定需要不同的程式引擎；請更新程式本身"
        );
        let mode = Mode::parse(&config.mode)?;
        ensure!(
            path.file_name().and_then(|n| n.to_str()) == Some(mode.config()),
            "模式設定檔名不符"
        );
        ensure!(valid_hash(&config.source_sha256), "字典來源雜湊格式不符");
        ensure!(
            !config.rules.is_empty() && config.rules.len() <= MAX_TABLE_RULES,
            "模式規則為空或超出上限"
        );
        let mut seen = BTreeMap::new();
        for (from, to) in config.rules {
            valid_rule(&from, &to)?;
            ensure!(seen.insert(from, to).is_none(), "模式包含重複規則");
        }
        let converter = ZhConverter::from_pairs_with_target_variant(mode.variant(), seen);
        Ok(Self {
            backend: Backend::External(Box::new(converter)),
            dictionary_version: ENGINE_VERSION.to_owned(),
            dictionary_hash: config.source_sha256,
        })
    }
    pub fn convert(&self, text: &str) -> Result<String> {
        ensure!(!text.contains('\0'), "名稱含有不合法的空字元");
        Ok(match &self.backend {
            Backend::Embedded(converter) => converter.convert(text),
            Backend::External(converter) => converter.convert(text),
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RulesConfig {
    schema: u32,
    engine_version: String,
    mode: String,
    source_sha256: String,
    rules: Vec<(String, String)>,
}
pub(crate) fn config_bytes(
    tables: &MediaWikiTables,
    mode: Mode,
    source_sha256: &str,
) -> Result<Vec<u8>> {
    let mut rules = tables
        .get("ZH_TO_HANT")
        .context("MediaWiki 來源缺少繁體表")?
        .clone();
    if mode == Mode::ZhTw {
        rules.extend(
            tables
                .get("ZH_TO_TW")
                .context("MediaWiki 來源缺少台灣表")?
                .clone(),
        );
    }
    Ok(serde_json::to_vec(&RulesConfig {
        schema: 1,
        engine_version: ENGINE_VERSION.to_owned(),
        mode: mode.name().to_owned(),
        source_sha256: source_sha256.to_owned(),
        rules: rules.into_iter().collect(),
    })?)
}
pub(crate) fn source_hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
pub(crate) fn valid_hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}
fn valid_rule(from: &str, to: &str) -> Result<()> {
    ensure!(
        !from.is_empty()
            && !to.is_empty()
            && from.len() <= MAX_RULE_BYTES
            && to.len() <= MAX_RULE_BYTES
            && !from.chars().chain(to.chars()).any(char::is_control),
        "MediaWiki 規則含有空字串、控制字元或過長文字"
    );
    Ok(())
}

/// Parses only constant string-to-string tables; never executes PHP.
/// Table names and override order follow zhconv 0.4.2 build.rs (MIT OR Apache-2.0):
/// https://github.com/Gowee/zhconv-rs/blob/v0.4.2-1/build.rs
/// Imported MediaWiki tables retain their GPL-2.0-or-later license.
pub fn parse_mediawiki(bytes: &[u8]) -> Result<MediaWikiTables> {
    ensure!(
        !bytes.is_empty() && bytes.len() as u64 <= MAX_SOURCE_BYTES,
        "MediaWiki 來源為空或超出上限"
    );
    let text = std::str::from_utf8(bytes).context("MediaWiki 來源不是 UTF-8")?;
    let mut parser = SourceParser { text, offset: 0 };
    parser.expect("<?php")?;
    parser.expect("namespace")?;
    parser.expect("MediaWiki\\Languages\\Data")?;
    parser.expect(";")?;
    parser.expect("class")?;
    parser.expect("ZhConversion")?;
    parser.expect("{")?;
    let mut tables = BTreeMap::new();
    loop {
        if parser.consume("}")? {
            break;
        }
        parser.expect("public")?;
        parser.expect("const")?;
        let name = parser.identifier()?;
        ensure!(
            TABLE_NAMES.contains(&name.as_str()),
            "新版 MediaWiki 有不支援的常數表：{name}"
        );
        parser.expect("=")?;
        parser.expect("[")?;
        let mut rules = BTreeMap::new();
        while !parser.consume("]")? {
            let from = parser.string()?;
            parser.expect("=>")?;
            let to = parser.string()?;
            valid_rule(&from, &to)?;
            ensure!(rules.insert(from, to).is_none(), "MediaWiki 表含有重複規則");
            ensure!(rules.len() <= MAX_TABLE_RULES, "MediaWiki 規則數超出上限");
            if !parser.consume(",")? {
                parser.expect("]")?;
                break;
            }
        }
        parser.expect(";")?;
        ensure!(!rules.is_empty(), "MediaWiki 常數表不能為空");
        ensure!(tables.insert(name, rules).is_none(), "MediaWiki 表名稱重複");
    }
    parser.skip()?;
    ensure!(
        parser.offset == text.len(),
        "MediaWiki 來源含有未支援的語法"
    );
    ensure!(
        tables.len() == TABLE_NAMES.len(),
        "MediaWiki 來源缺少必要的常數表"
    );
    Ok(tables)
}
struct SourceParser<'a> {
    text: &'a str,
    offset: usize,
}
impl SourceParser<'_> {
    fn skip(&mut self) -> Result<()> {
        loop {
            let remaining = &self.text[self.offset..];
            if let Some(first) = remaining.chars().next()
                && first.is_whitespace()
            {
                self.offset += first.len_utf8();
                continue;
            }
            if let Some(body) = remaining.strip_prefix("/*") {
                // Like PHP, search for the terminator after the opening `/*`, so `/*/`
                // does not close itself.
                let end = body.find("*/").context("MediaWiki 註解未結束")?;
                self.offset += "/*".len() + end + "*/".len();
                continue;
            }
            if remaining.starts_with("//") || remaining.starts_with('#') {
                self.offset += remaining.find('\n').unwrap_or(remaining.len());
                continue;
            }
            return Ok(());
        }
    }
    fn consume(&mut self, token: &str) -> Result<bool> {
        self.skip()?;
        let remaining = &self.text[self.offset..];
        let word_boundary = token
            .chars()
            .last()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
            && remaining
                .get(token.len()..)
                .and_then(|tail| tail.chars().next())
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        if remaining.starts_with(token) && !word_boundary {
            self.offset += token.len();
            Ok(true)
        } else {
            Ok(false)
        }
    }
    fn expect(&mut self, token: &str) -> Result<()> {
        ensure!(self.consume(token)?, "MediaWiki 語法不相容，預期 {token}");
        Ok(())
    }
    fn identifier(&mut self) -> Result<String> {
        self.skip()?;
        let length = self.text[self.offset..]
            .bytes()
            .take_while(|b| b.is_ascii_uppercase() || *b == b'_')
            .count();
        ensure!(length > 0, "MediaWiki 常數名稱缺失");
        let result = self.text[self.offset..self.offset + length].to_owned();
        self.offset += length;
        Ok(result)
    }
    fn string(&mut self) -> Result<String> {
        self.expect("'")?;
        let mut result = String::new();
        loop {
            let character = self.text[self.offset..]
                .chars()
                .next()
                .context("MediaWiki 字串未結束")?;
            self.offset += character.len_utf8();
            match character {
                '\'' => return Ok(result),
                '\\' => {
                    let escaped = self.text[self.offset..]
                        .chars()
                        .next()
                        .context("MediaWiki 字串跳脫未結束")?;
                    ensure!(
                        matches!(escaped, '\\' | '\''),
                        "MediaWiki 字串有不支援的跳脫語法"
                    );
                    self.offset += escaped.len_utf8();
                    result.push(escaped);
                }
                character => result.push(character),
            }
            ensure!(result.len() <= MAX_RULE_BYTES, "MediaWiki 規則文字超出上限");
        }
    }
}
