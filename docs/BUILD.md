# 建置、下載與來源重建

## 環境需求

- Windows x64；Rust／MSVC 工具鏈與 Windows SDK。
- 本專案驗證工具鏈固定為 Rust `1.98.0`、target `x86_64-pc-windows-msvc`。
- 打包與發布工具使用 Python `3.11` 以上，僅依賴標準函式庫。執行產品不需要 Python。
- GitHub CLI 範例使用已登入的 `gh`；亦可從 Release 頁面直接下載公開附件。
- FAT32／exFAT 手動檢查需要系統管理員權限（以 `diskpart` 建立暫時 VHDX）；其他測試不需要。

## 下載附件與存放位置

```powershell
chcp 65001 > $null
$ErrorActionPreference = 'Stop'
$taskDownloads = 'C:\Codex\SC2TC-Renamer\downloads\v1.1.0'
$taskReleaseDirectory = 'C:\Codex\SC2TC-Renamer\releases\v1.1.0'
if (Test-Path -LiteralPath $taskDownloads) { throw '請使用尚不存在的下載目錄。' }
if (Test-Path -LiteralPath $taskReleaseDirectory) { throw '請使用尚不存在的解壓目錄。' }
New-Item -ItemType Directory -Path $taskDownloads | Out-Null
gh auth status
if ($LASTEXITCODE -ne 0) { throw '使用 CLI 下載前，請先登入 GitHub 帳號。' }
gh release download v1.1.0 --repo marvin3991/SC2TC-Renamer --dir $taskDownloads
if ($LASTEXITCODE -ne 0) { throw '附件下載失敗；請確認版本已發布與網路連線。' }
Get-FileHash -LiteralPath (Join-Path $taskDownloads 'SC2TC-Renamer-v1.1.0-portable.zip'), (Join-Path $taskDownloads 'SC2TC-Renamer-v1.1.0-source.zip') -Algorithm SHA256
# 先核對 SHA256SUMS.txt，再解壓。
Expand-Archive -LiteralPath (Join-Path $taskDownloads 'SC2TC-Renamer-v1.1.0-portable.zip') -DestinationPath $taskReleaseDirectory
Expand-Archive -LiteralPath (Join-Path $taskDownloads 'SC2TC-Renamer-v1.1.0-source.zip') -DestinationPath $taskReleaseDirectory
```

| 資料 | 本例路徑 |
|------|----------|
| 原始下載附件 | `C:\Codex\SC2TC-Renamer\downloads\v1.1.0\` |
| 執行檔 | `C:\Codex\SC2TC-Renamer\releases\v1.1.0\SC2TC-Renamer-1.1.0-portable\SC2TC-Renamer.exe` |
| 完整對應原始碼根目錄 | `C:\Codex\SC2TC-Renamer\releases\v1.1.0\SC2TC-Renamer-1.1.0-source\` |
| 鎖定相依原始碼 | 上述來源根目錄的 `vendor\rust\` |
| MediaWiki 表與生成器 | 上述來源根目錄的 `vendor\mediawiki\` |

路徑可以自行選擇。更換版本時，同步修改指令與附件名稱；不使用覆蓋參數。`SHA256SUMS.txt` 只列 Release 附件，可在下載資料夾以 `sha256sum -c SHA256SUMS.txt`（Git Bash 等）一次核對；解壓後的 `SC2TC-Renamer.exe` 雜湊請與 `release-manifest.json` 的 `sha256` 欄位核對。

## 本機開發與測試

在專案根目錄執行：

```powershell
chcp 65001 > $null
cargo fmt --all --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --locked -- --test-threads=1
py -3 -m unittest discover -s tests -p 'test_*.py' -v
cargo build --release --locked --target x86_64-pc-windows-msvc
```

Rust 測試涵蓋名稱轉換、遞迴掃描、同名衝突、身分變更、紀錄中斷及復原、路徑選取與更新包驗證。Python 測試驗證發布附件清單、版本與來源配對、壓縮包路徑安全、Release 發布流程，以及本機打包的來源快照檢查。

- **`--test-threads=1` 的原因**：改名、復原與轉換表切換都會取得全域具名 mutex（作業鎖 `Local\OpenCCRenamerOperationsV1`，名稱與舊版相容；轉換表另有字典鎖）。Win32 mutex 的擁有權屬於執行緒，libtest 平行執行時測試會互搶而偶發失敗。請不要省略這個參數，也不要在 GUI 正在改名或復原時跑測試。
- **測試隔離**：各 Rust 測試二進位開頭以 `Store::override_standard_root` 把轉換表儲存位置改到 `work/<測試檔>-dictionary-store`，`--self-test` 也改用測試資料夾內的 `dictionary-store`，因此只使用內附轉換表，不讀寫使用者在 `%LOCALAPPDATA%` 套用的轉換表設定。掃描會確保 `%LOCALAPPDATA%\SC2TC-Renamer\history\` 存在（只建立資料夾，用於排除紀錄目錄）。測試與自我檢查的檔案操作只使用 `work/` 下的合成資料。
- **`work/` 的清理**：每個測試在 `work/` 下使用各自的子資料夾，測試通過時刪除、失敗時保留供查核。沒有測試、建置或自我檢查正在執行時，整個 `work/` 可以隨時刪除（已列入 `.gitignore`）。
- **SUBST 殘留**：磁碟根目錄測試會把 `work/` 下的資料夾暫時映射到 F–Z 中第一個可用的磁碟代號，結束時自動移除。測試程序被強制結束時映射可能殘留；以 `subst` 列出目前映射，確認是指向本專案 `work\` 的代號後，以 `subst X: /D`（`X` 換成該代號）手動移除。
- **相依變更**：修改 `Cargo.toml` 或 `Cargo.lock` 後，重新執行 `py -3 scripts/collect-licenses.py` 並提交 `licenses/` 的變更；CI 會重新產生並以 `git status --porcelain -- licenses` 比對，不一致時失敗。

執行 Release 版命令列檢查時，以 `Start-Process -Wait` 等待 Windows GUI 子系統程式完成。`--self-test` 只建立最後一層資料夾，所以先建立 `work\`：

```powershell
chcp 65001 > $null
$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force -Path (Join-Path $PWD 'work') | Out-Null
$taskExecutable = (Resolve-Path -LiteralPath '.\target\x86_64-pc-windows-msvc\release\SC2TC-Renamer.exe').Path
$taskFixture = Join-Path $PWD ('work\check-' + [guid]::NewGuid().ToString('N'))
$taskProcess = Start-Process -FilePath $taskExecutable -ArgumentList @('--self-test', ('"' + $taskFixture + '"')) -WindowStyle Hidden -Wait -PassThru
if ($taskProcess.ExitCode -ne 0) { throw ('自我檢查失敗，請查看 ' + $taskFixture + '.failure.json。') }
Get-Content -LiteralPath (Join-Path $taskFixture 'verification.json') -Encoding UTF8
```

## 命令列參數

一般啟動不需要參數；拖曳檔案到執行檔圖示也會以一般 GUI 開啟，錯誤以對話框顯示。下列旗標必須是第一個參數，第二個參數為輸出位置：

| 參數 | 用途 | 網路 |
|------|------|------|
| `--self-test <尚不存在的資料夾>` | 在該資料夾建立合成檔案，以兩種模式實際改名並復原，結果寫入 `verification.json`；使用資料夾內的隔離字典儲存與內附轉換表 | 不需要 |
| `--ui-self-check <尚不存在的資料夾>` | 以合成預覽開啟視窗，切換到最小尺寸後檢查底部按鈕可見與實際尺寸，並記錄中文字型是否載入，結果寫入 `ui-verification.json`；手動診斷用，CI 未執行 | 不需要 |
| `--self-test-update <尚不存在的資料夾>` | 從 crates.io 下載並驗證 zhconv 正式版，在資料夾內的隔離儲存套用後再回復內附表，結果寫入 `update-verification.json` | 需要 |
| `--check-dictionary-update <JSON 路徑>` | 查詢 crates.io 上 zhconv 的最新正式版資訊並寫入該 JSON | 需要 |

旗標後有輸出路徑但執行失敗時，錯誤寫入同一位置的 `<路徑>.failure.json`（例如 `work\check-<識別碼>.failure.json`），不跳出對話框；旗標缺少輸出路徑時只輸出到主控台並以代碼 2 結束。Release 版沒有主控台，請以 `Start-Process -Wait -PassThru` 取得結束代碼。

## FAT32／exFAT 手動檢查

CI 與一般測試只使用 NTFS。FAT 類檔案系統的根目錄身分與改名後身分變化，請以系統管理員 PowerShell 在暫時 VHDX 上檢查；不要用實際的隨身碟或資料磁碟。下例使用代號 `T:`，請先確認未被占用；VHDX 路徑只用 ASCII 字元。

```powershell
chcp 65001 > $null
$ErrorActionPreference = 'Stop'
$taskVhd = Join-Path $PWD 'work\fat-check.vhdx'
$taskScript = Join-Path $PWD 'work\fat-check-diskpart.txt'
if (Test-Path -LiteralPath $taskVhd) { throw '請使用尚不存在的 VHDX 路徑。' }
@(
    "create vdisk file=""$taskVhd"" maximum=1024 type=expandable",
    'attach vdisk',
    'create partition primary',
    'format fs=fat32 quick label=SC2TCFAT',
    'assign letter=T'
) | Set-Content -LiteralPath $taskScript -Encoding ascii
diskpart /s $taskScript
if ($LASTEXITCODE -ne 0) { throw 'diskpart 建立 VHDX 失敗。' }
```

建立後執行 `--self-test T:\sc2tc-check`（同上節以 `Start-Process -Wait` 等待），並在 GUI 加入 `T:\` 根目錄，以 `zh-TW` 對含 `U盘`、`博客` 等會變長的名稱掃描、改名再復原。完成後以 `select vdisk file="<VHDX 路徑>"`、`detach vdisk` 卸載，再刪除 `work\` 下的 VHDX。exFAT 把 `fs=fat32` 改為 `fs=exfat` 重做一次。

## 打包與發布

執行 `build.ps1` 會建立固定來源快照、收齊鎖定相依套件及授權，以該快照離線建置，再產生 portable ZIP 與完整 source ZIP。產出位於新的 `dist/rust-<版本>-<識別碼>/`；建置紀錄與雜湊保留於同一目錄，不覆蓋既有套件。

本機執行 `build.ps1` 前，工作目錄須保持乾淨：

- 快照直接收入 `src/`、`assets/`、`licenses/`、`docs/`、`examples/`、`vendor/mediawiki/` 與 `tests/*.rs` 等路徑下的所有檔案，不參考 `.gitignore`。遇到 `Thumbs.db`、`Desktop.ini`、`.DS_Store`、以 `~$` 開頭的檔案（不分大小寫），以及 `.env*`、`.exe`、`.log` 時停止；請先移除再打包。
- 不要在專案根目錄預先執行 `cargo vendor`。根目錄已有 `vendor/rust/` 時，打包會略過 vendoring，並要求 `.cargo/config.toml` 含 `[source.crates-io] replace-with = "vendored-sources"` 與 `[source.vendored-sources] directory = "vendor/rust"`，否則停止；本 repo 的設定檔不含這兩段，正常流程由打包腳本在快照內 vendoring 並補上設定。

Release 由 `v<版本>` tag 觸發 GitHub Actions，僅接受已合併到 `main` 且與 `Cargo.toml` 版本一致的來源。`CHANGELOG.md` 必須有該版本的 `## [<版本>]` 段落且內容非空，否則在建置前就停止；Release 說明由該段落加上固定的來源與授權說明產生。若事先手動建立了同版本的 draft，發布流程沿用 draft 既有的說明，不覆寫。測試、執行檔自我檢查、來源包安全與離線檢查通過後，才將全部附件發布為同一個 Release。原始碼與執行檔由同份快照產生；原始碼 ZIP 放在 Release 附件，不進 Git 版控。

### 發布中斷後的重跑

每次建置的 ZIP 位元組都不同，所以重跑不能沿用前一次上傳的附件：

| 重跑時的 Release 狀態 | 行為 |
|------|------|
| 尚無 Release | 建立 draft、上傳全部附件、核對後發布 |
| draft（前次中斷） | 名稱相同但未上傳完成或大小／SHA-256 不符的附件先刪除再重新上傳；缺少的附件補傳；全部核對通過才發布 |
| 已發布 | 只讀取核對，不刪除、不上傳；附件與本次建置不符時以代碼 3 結束，訊息為「Release is already published…」 |

draft 內多出計畫外的附件時仍會停止，需手動從 draft 移除後再重跑。已發布的版本不要重跑同一個 tag；要修正內容請發布新版本。

## 完整來源包離線重建

安裝上述通用工具鏈後，在 `SC2TC-Renamer-<版本>-source\` 執行：

```powershell
chcp 65001 > $null
cargo build --release --locked --offline --target x86_64-pc-windows-msvc
```

包內 `.cargo/config.toml` 指向 `vendor/rust/`，不需要連線 registry。發布前會用空 Cargo 快取及獨立 target 目錄執行離線檢查，結果見 `source-verification.json`。Windows SDK、Rust／MSVC 與系統函式庫需另行安裝。

完整來源保留上游原始碼、授權全文、著作權聲明、表原始資料及生成器。第三方元件的可選資料與功能也依上游來源保留；實際轉換僅啟用 `mediawiki-hant`、`mediawiki-tw`。
