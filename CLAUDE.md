# CLAUDE.md

UEFI 開機前程式（Rust），在 Windows Boot Manager 啟動前把設定的實體記憶體範圍標記為不可用，再 chainload Windows。完整設計見 `DESIGN.md`。

## 指令

- 安裝 target：`rustup target add x86_64-unknown-uefi`
- 建置：`cargo build -p badram-efi --target x86_64-unknown-uefi --release`
- 測試純邏輯：`cargo test -p badram-core`
- QEMU 測試：需要 OVMF（見 DESIGN.md §7）

## 規則

- `badram-core` 保持 `no_std`、不依賴 `uefi` crate，所有可測的邏輯放這裡。
- 申請記憶體只能用 `EfiUnusableMemory` 或 `EfiReservedMemoryType`，絕不能用會在 ExitBootServices 後被回收的類型。
- 任何錯誤都不能讓程式默默繼續：一律依 `on_error` 處理並在畫面顯示。
- 提交到 git 的檔案（程式碼註解、README、commit message 等）不可包含機器專屬資訊：電腦型號、硬體配置、實際壞區位址、主機名稱、本機路徑。這些只放 `CLAUDE.local.md`。
