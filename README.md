# PongBiphang Schema (`pbps`)

宣告式資料庫 schema 版控工具。你維護一份「schema 該長什麼樣」，其餘的它負責。

- 設計規格：[docs/SPEC.md](docs/SPEC.md)
- 決策紀錄：[docs/ADR-0001-yaml-crate.md](docs/ADR-0001-yaml-crate.md)

目前處於 **Phase 0**（地基），尚無可用的執行檔。

## 開發環境

主要開發與發行目標是 **Linux**；Windows 的覆蓋由 CI matrix 負責。

```bash
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all
```

### Windows 上開發請用 WSL2

不要用 `x86_64-pc-windows-gnu`。rustup 的 self-contained mingw 沒有附 GNU
組譯器，`windows-sys` 這類使用 raw-dylib 的 crate 會編譯失敗 —— 而 `clap`
與 `miette` 都會拉進 `windows-sys`，所以任何有彩色輸出的 CLI 都躲不掉。
`gnullvm` 同樣需要外部的 llvm-mingw。

在 Windows 上有兩條可行的路：

1. **WSL2**（本專案採用）：`sudo apt install build-essential pkg-config`，
   再以 rustup 安裝 stable。專案要放在 WSL 檔案系統內（`~/pbps`），
   不要放 `/mnt/c` —— 那是 9p 檔案系統，cargo 的大量小檔 I/O 會明顯變慢。
2. **MSVC**：安裝 Visual Studio Build Tools 的 VC++ workload。

### 換行符

版控內一律 LF（見 `.gitattributes`）。這個工具自己會寫檔（`pbps fmt`），
若讓平台決定換行符，「工具擁有檔案格式」這個保證會在 Windows 上破功，
也會製造假的 git diff。

## 授權

MIT OR Apache-2.0
