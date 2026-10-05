# 建置、下載與來源重建

## 環境需求

- Windows x64；Rust／MSVC 工具鏈與 Windows SDK。
- 本專案驗證工具鏈固定為 Rust `1.98.0`、target `x86_64-pc-windows-msvc`。
- 打包與發布工具使用 Python `3.11` 以上，僅依賴標準函式庫。執行產品不需要 Python。
- GitHub CLI 範例使用已登入的 `gh`；亦可從 Release 頁面直接下載公開附件。

## 下載附件與存放位置

```powershell
chcp 65001 > $null
$ErrorActionPreference = 'Stop'
$taskDownloads = 'C:\Codex\SC2TC-Renamer\downloads\v1.0.0'
$taskReleaseDirectory = 'C:\Codex\SC2TC-Renamer\releases\v1.0.0'
if (Test-Path -LiteralPath $taskDownloads) { throw '請使用尚不存在的下載目錄。' }
if (Test-Path -LiteralPath $taskReleaseDirectory) { throw '請使用尚不存在的解壓目錄。' }
New-Item -ItemType Directory -Path $taskDownloads | Out-Null
gh auth status
if ($LASTEXITCODE -ne 0) { throw '使用 CLI 下載前，請先登入 GitHub 帳號。' }
gh release download v1.0.0 --repo marvin3991/SC2TC-Renamer --dir $taskDownloads
if ($LASTEXITCODE -ne 0) { throw '附件下載失敗；請確認版本已發布與網路連線。' }
Get-FileHash -LiteralPath (Join-Path $taskDownloads 'SC2TC-Renamer-v1.0.0-portable.zip'), (Join-Path $taskDownloads 'SC2TC-Renamer-v1.0.0-source.zip') -Algorithm SHA256
# 先核對 SHA256SUMS.txt，再解壓。
Expand-Archive -LiteralPath (Join-Path $taskDownloads 'SC2TC-Renamer-v1.0.0-portable.zip') -DestinationPath $taskReleaseDirectory
Expand-Archive -LiteralPath (Join-Path $taskDownloads 'SC2TC-Renamer-v1.0.0-source.zip') -DestinationPath $taskReleaseDirectory
```

| 資料 | 本例路徑 |
|------|----------|
| 原始下載附件 | `C:\Codex\SC2TC-Renamer\downloads\v1.0.0\` |
| 執行檔 | `C:\Codex\SC2TC-Renamer\releases\v1.0.0\SC2TC-Renamer-1.0.0-portable\SC2TC-Renamer.exe` |
| 完整對應原始碼根目錄 | `C:\Codex\SC2TC-Renamer\releases\v1.0.0\SC2TC-Renamer-1.0.0-source\` |
| 鎖定相依原始碼 | 上述來源根目錄的 `vendor\rust\` |
| MediaWiki 表與生成器 | 上述來源根目錄的 `vendor\mediawiki\` |

路徑可以自行選擇。更換版本時，同步修改指令與附件名稱；不使用覆蓋參數。

## 本機開發與測試

在專案根目錄執行：

```powershell
chcp 65001 > $null
cargo fmt --all --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --locked -- --test-threads=1
py -3 -m unittest discover -s tests -p test_release.py -v
cargo build --release --locked --target x86_64-pc-windows-msvc
```

Rust 測試涵蓋名稱轉換、遞迴掃描、同名衝突、身分變更、紀錄中斷及復原、路徑選取與更新包驗證。Python 測試驗證發布附件清單、版本與來源配對、壓縮包路徑安全及 Release 發布流程。

測試與自我檢查只使用 `work/` 下的合成資料。執行 Release 版命令列檢查時，以 `Start-Process -Wait` 等待 Windows GUI 子系統程式完成：

```powershell
chcp 65001 > $null
$ErrorActionPreference = 'Stop'
$taskExecutable = (Resolve-Path -LiteralPath '.\target\x86_64-pc-windows-msvc\release\SC2TC-Renamer.exe').Path
$taskFixture = Join-Path $PWD ('work\check-' + [guid]::NewGuid().ToString('N'))
$taskProcess = Start-Process -FilePath $taskExecutable -ArgumentList @('--self-test', ('"' + $taskFixture + '"')) -WindowStyle Hidden -Wait -PassThru
if ($taskProcess.ExitCode -ne 0) { throw '自我檢查失敗，請保留輸出紀錄查核。' }
Get-Content -LiteralPath (Join-Path $taskFixture 'verification.json') -Encoding UTF8
```

## 打包與發布

執行 `build.ps1` 會建立固定來源快照、收齊鎖定相依套件及授權，以該快照離線建置，再產生 portable ZIP 與完整 source ZIP。產出位於新的 `dist/rust-<版本>-<識別碼>/`；建置紀錄與雜湊保留於同一目錄，不覆蓋既有套件。

Release 由 `v<版本>` tag 觸發 GitHub Actions，僅接受已合併到 `main` 且與 `Cargo.toml` 版本一致的來源。測試、執行檔自我檢查、來源包安全與離線檢查通過後，才將全部附件發布為同一個 Release。原始碼與執行檔由同份快照產生；原始碼 ZIP 放在 Release 附件，不進 Git 版控。

## 完整來源包離線重建

安裝上述通用工具鏈後，在 `SC2TC-Renamer-<版本>-source\` 執行：

```powershell
chcp 65001 > $null
cargo build --release --locked --offline --target x86_64-pc-windows-msvc
```

包內 `.cargo/config.toml` 指向 `vendor/rust/`，不需要連線 registry。發布前會用空 Cargo 快取及獨立 target 目錄執行離線檢查，結果見 `source-verification.json`。Windows SDK、Rust／MSVC 與系統函式庫需另行安裝。

完整來源保留上游原始碼、授權全文、著作權聲明、表原始資料及生成器。第三方元件的可選資料與功能也依上游來源保留；實際轉換僅啟用 `mediawiki-hant`、`mediawiki-tw`。
