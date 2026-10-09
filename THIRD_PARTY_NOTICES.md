# 第三方授權聲明

SC2TC-Renamer 自有程式碼採 [Apache License 2.0](LICENSE)。整體執行檔包含 MediaWiki 的 `GPL-2.0-or-later` 中文轉換表，選擇依 `GPL-3.0-only` 散布，全文見 [COPYING](COPYING)。第三方元件保留各自授權，不能將函式庫授權誤認為資料表授權。

## 元件與來源

| 元件 | 授權與本版用途 | 授權文件 |
|------|----------------|----------|
| zhconv `0.4.2` | 函式庫為 `MIT OR Apache-2.0`，本專案選 Apache 2.0；僅啟用 `mediawiki-hant`、`mediawiki-tw` | `licenses/rust/zhconv-0.4.2/` |
| MediaWiki 中文轉換表 | `GPL-2.0-or-later`；整體執行檔選擇依相容的 GPL 第 3 版散布 | `licenses/mediawiki/` |
| Rust 相依套件 | 鎖定的 Windows 建置與執行相依，依各套件授權散布 | `licenses/rust-inventory.json`、`licenses/rust/`、`licenses/upstream/` |
| 內附備援字型（`epaint_default_fonts 0.33.3`，內嵌於執行檔） | Hack Regular：MIT，並含 Bitstream Vera 授權條款（`Hack-Regular.txt`）；emoji-icon-font：MIT（`emoji-icon-font-mit-license.txt`）；Noto Emoji：SIL Open Font License 1.1（`OFL.txt`）；Ubuntu Light：Ubuntu Font Licence 1.0（`UFL.txt`）；套件程式碼為 `MIT OR Apache-2.0`（`licenses/upstream/egui/`） | `licenses/rust/epaint_default_fonts-0.33.3/fonts/` |

實際相依、版本及授權檔由 `scripts/collect-licenses.py` 依 `cargo metadata --locked` 建立。Windows 系統字型只在使用者電腦上讀取，不隨本專案重新散布。

原始 PHP 轉換表僅作為資料讀取，不執行 PHP。固定來源提交、下載連結與 SHA-256 見 `licenses/mediawiki/provenance.json`、`vendor/mediawiki/manifest.json`。對應來源包內保留 `vendor/rust/zhconv-0.4.2/data/ZhConversion.php`、完整上游來源及授權，並附 `vendor/mediawiki/upstream-source/` 的原始表與生成器。

## 對應原始碼與再次散布

`build.ps1` 從當時工作檔建立固定快照，收齊鎖定相依來源，再以該快照離線建置，產生同版本的 `*-portable.zip` 與 `*-source.zip`。來源包包含實際建置的程式、`Cargo.lock`、轉換表、圖標與其產生程式 `examples/prepare_icon.rs`、建置腳本及相依套件。

發布或再次交付執行檔時，必須提供該版本完整來源包及授權文件。私人 repo 網址、不同版本的來源或上游連結不能取代對應原始碼交付。GitHub 自動產生的來源 ZIP 不包含 Actions 取得的完整相依來源，請使用本專案的來源附件。

來源包可使用已安裝的 Rust／MSVC 工具鏈與 Windows SDK 離線重建。通用工具與系統函式庫須另行安裝；步驟見 [建置文件](docs/BUILD.md)。相關要求見 [GPL 第 3 版第 1、6 節](https://www.gnu.org/licenses/gpl-3.0.en.html)。

完整第三方來源也包含上游未啟用功能的資料與程式碼；本專案保留其原始著作權、來源及授權聲明，實際啟用功能由 `Cargo.toml`、來源紀錄與相依清單確定。不能因某功能未啟用，就刪去仍隨來源包提供之檔案的必要署名。

## 圖標

圖標由內建 image_gen 工具生成；Windows ICO 是 PNG 的格式轉換與尺寸縮放版本，由 `examples/prepare_icon.rs` 從 `assets/logo-v2.png` 產生 16～256 像素共 7 種尺寸（`cargo run --example prepare_icon`；程式以 `create_new` 寫入，重新產生前須先移除既有的 `assets/app.ico`）。圖標資源位於 `assets/`，設計描述見 `assets/logo-prompt.md`。本專案名稱與圖標不代表 MediaWiki 或 zhconv 官方背書。
