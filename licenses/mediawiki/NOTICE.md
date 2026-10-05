# MediaWiki 中文轉換表

Copyright MediaWiki contributors.

本版使用 `zhconv 0.4.2` 隨附的 `data/ZhConversion.php`，其授權為 `GPL-2.0-or-later`。`zhconv` 函式庫原始碼本身為 `MIT OR Apache-2.0`，不能將函式庫授權誤當成轉換表授權。上游授權說明見 [zhconv README](https://github.com/Gowee/zhconv-rs#license)。

本專案保留原 GPL 第 2 版全文於 `LICENSE-GPL-2.0.txt`，整體 Rust 執行檔選擇依相容的 GNU GPL 第 3 版散布，全文在根目錄 `COPYING`。作者自己的原始碼仍以根目錄 `LICENSE` 的 Apache 2.0 提供。

`provenance.json` 記錄鎖定的 zhconv 版本、啟用的功能、MediaWiki 來源提交與原始表 SHA-256。對應原始碼包保留完整 `vendor/rust/zhconv-0.4.2`，包括原字表、建置腳本和上游授權全文。

套用未來轉換表更新時，必須保留當次上游 GPL 授權與來源檔；再次發布含更新表的執行檔時，也要提供該版本的對應原始碼。
