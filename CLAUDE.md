# 專案工作規則

- 一律用繁體中文回覆；程式碼、路徑與產品名稱維持原文。
- 本工具離線轉換檔案與資料夾名稱，不改寫文件內容，不覆蓋同名項目。
- 改名前必須完成預覽、原名稱備份及確認；保留可追蹤的執行與復原紀錄。
- 測試只能使用 `work/` 下的合成資料；磁碟根目錄測試使用暫時 SUBST 映射，不能對使用者的實際磁碟執行改名。
- 修改前閱讀受影響的程式、介面、設定與測試；交件前執行測試並檢查 diff。
- commit 與 PR 不加 AI 工具署名；保留人類協作者署名。不要使用 `--no-verify`。
- Git 設定只使用 repo local 範圍；未經使用者明確授權，不 force-push 或改寫已推送歷史。
- Rust 驗證：`cargo fmt --all --check`、`cargo clippy --all-targets --all-features --locked -- -D warnings`、`cargo test --locked -- --test-threads=1`。
- Python 僅供打包、發布及授權收集腳本使用，使用標準函式庫；產品執行檔不依賴 Python。
- 修改 `Cargo.toml` 或 `Cargo.lock` 後，必須重新執行 `scripts/collect-licenses.py` 並提交 `licenses/` 的變更；CI 會比對，過期即失敗。
- 引擎使用 zhconv-rs 的 MediaWiki 轉換表；啟用功能僅限 `mediawiki-hant`、`mediawiki-tw`。
- 轉換表更新只追蹤 crates.io 的 zhconv 正式版本，驗證官方雜湊與相容性後由使用者確認套用；保留舊設定，不自動更新。
- 既有名稱備份保留原始紀錄位置及逐筆狀態；只能供復原，新改名必須重新掃描。
- 自有原始碼採 Apache-2.0；整體執行檔依 GPL-3.0-only 發布，附同版本完整對應原始碼與第三方授權。
- 第三方原始碼中的授權、著作權與來源聲明依上游保留，不因產品改名而抹除。
