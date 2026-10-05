# MediaWiki 轉換表的上游來源

此目錄保留 `SOURCE-MANIFEST.json` 所列 MediaWiki 同一提交的原始轉換表、產生腳本及著作權資訊。內附的 `*.manual` 是適合修改的上游來源；`ZhConversion.php` 是上游的產生結果，也是本專案實際讀取的資料。

建置 SC2TC-Renamer 不需執行此目錄的程式；對照 PHP 檔只當成資料處理。使用 `cargo build --release --locked --offline` 時，直接使用 `vendor/mediawiki/ZhConversion.php` 與 vendored zhconv 相依套件。

若要重新產生整份上游 MediaWiki 表，請先閱讀 `maintenance/language/zhtable/README` 與 `Makefile.py`。上游產生腳本還會讀取指定版本的 Unihan、SCIM 與 libtabe 資料，這些選用的上游再產生步驟需要另行取得其輸入；不屬於重建本版執行檔的必要步驟。不要對使用者的資料目錄執行上游產生腳本。

MediaWiki 原始授權及著作權名單保存在 `COPYING` 與 `CREDITS`；本目錄資料適用 MediaWiki 原有授權。`.gitattributes` 將本目錄標記為 `-text`，Git 不會改寫上游原始行尾；`SOURCE-MANIFEST.json` 的 SHA-256 因此可以跨 Windows checkout 核對。專案讀取的 PHP 表另以 `*.php text eol=lf` 固定 LF 行尾。
