//! 實際渲染各類載入錯誤，用來檢視診斷品質。
//!
//! `cargo run -p pbps-load --example diagnostics`

use std::path::Path;

fn show(title: &str, yaml: &str) {
    println!("\n\x1b[1m### {title}\x1b[0m");
    match pbps_load::load_table_str(Path::new("schema/dbo.客戶.yml"), yaml) {
        Ok(t) => println!("（載入成功：{}）", t.name),
        Err(errs) => {
            for e in errs {
                println!("{:?}", miette::Report::new(e));
            }
        }
    }
}

fn main() {
    show(
        "語意錯誤：型別括號未閉合（檔案含中文，驗證位元組偏移）",
        "table: dbo.客戶\ndescription: 這是一張中文描述的客戶主檔\ncolumns:\n  客戶編號:\n    type: bigint\n  餘額:\n    type: \"decimal(18, 2\"\n    nullable: false\n",
    );

    show(
        "語意錯誤：表名未限定 schema",
        "table: customer\ncolumns:\n  a:\n    type: int\n",
    );

    show(
        "語意錯誤：外鍵目標格式錯誤",
        "table: dbo.t\ncolumns:\n  a:\n    type: int\nforeign_keys:\n  fk_a:\n    columns: [a]\n    references: dbo.region\n",
    );

    show(
        "結構錯誤：欄位名拼錯",
        "table: dbo.t\ncolumns:\n  a:\n    type: int\n    nulable: false\n",
    );

    show(
        "結構錯誤：重複的欄位名",
        "table: dbo.t\ncolumns:\n  email:\n    type: nvarchar(255)\n  email:\n    type: varchar(50)\n",
    );
}
