use crate::{
    converter::{Converter, ENGINE_VERSION, Mode},
    engine::{self, Status},
    journal::{self, Journal},
};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

pub fn snapshots(root: &Path) -> Result<BTreeMap<String, String>> {
    let mut map = BTreeMap::new();
    let mut pending = vec![root.to_owned()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let digest = Sha256::digest(fs::read(&path)?)
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>();
                map.insert(
                    path.strip_prefix(root)?.to_string_lossy().into_owned(),
                    digest,
                );
            }
        }
    }
    Ok(map)
}
pub fn self_test(directory: &Path) -> Result<Value> {
    fs::create_dir(directory)?;
    let cancel = AtomicBool::new(false);
    let mut modes = vec![];
    let mut total_renamed = 0;
    let mut total_restored = 0;
    for mode in [Mode::ZhHant, Mode::ZhTw] {
        let root = directory.join(mode.name());
        fs::create_dir(&root)?;
        fs::create_dir_all(root.join("简体目录/第二层文件夹"))?;
        fs::write(
            root.join("简体目录/第二层文件夹/软件资料.txt"),
            b"synthetic document only",
        )?;
        for (name, bytes) in [
            ("岳飞传.txt", b"simplified Yue Fei".as_slice()),
            ("岳飛照片.txt", b"traditional Yue Fei".as_slice()),
            ("于谦资料.txt", b"simplified Yu Qian".as_slice()),
            ("于謙照片.txt", b"traditional Yu Qian".as_slice()),
            ("岳飛的软件资料.txt", b"mixed filename".as_slice()),
            ("报告.txt", b"original simplified".as_slice()),
            ("報告.txt", b"original traditional".as_slice()),
        ] {
            fs::write(root.join(name), bytes)?;
        }
        let converter = Converter::with_mode(mode)?;
        let expected_terms = match mode {
            Mode::ZhHant => "軟件 數據庫 鼠標",
            Mode::ZhTw => "軟體 資料庫 滑鼠",
        };
        ensure!(
            converter.convert("软件 数据库 鼠标")? == expected_terms,
            "{} terminology incorrect",
            mode.name()
        );
        let mut samples = BTreeMap::new();
        for input in ["岳飞", "岳飛", "于谦", "于謙", "岳飛的软件资料"] {
            let converted = converter.convert(input)?;
            ensure!(
                converter.convert(&converted)? == converted,
                "{} second conversion changed {input}",
                mode.name()
            );
            samples.insert(input, converted);
        }
        ensure!(samples["岳飞"] == "岳飛", "Yue Fei conversion incorrect");
        ensure!(samples["岳飛"] == "岳飛", "traditional Yue Fei changed");
        ensure!(samples["于谦"] == "于謙", "Yu Qian conversion incorrect");
        ensure!(samples["于謙"] == "于謙", "traditional Yu Qian changed");
        ensure!(
            samples["岳飛的软件资料"].starts_with("岳飛的"),
            "name changed in mixed filename"
        );
        let before = snapshots(&root)?;
        let plan =
            engine::make_plan_with_mode(std::slice::from_ref(&root), mode, &cancel, &|_| {})?;
        ensure!(
            plan.rows
                .iter()
                .filter(|r| r.status == Status::Conflict)
                .count()
                == 1,
            "collision not flagged"
        );
        let journal = Journal::create(&directory.join("history"), &plan)?;
        let renamed = journal::apply(&plan, &journal, &cancel, &|_| {})?;
        let after = snapshots(&root)?;
        let mut before_hashes = before.values().collect::<Vec<_>>();
        let mut after_hashes = after.values().collect::<Vec<_>>();
        before_hashes.sort();
        after_hashes.sort();
        ensure!(before_hashes == after_hashes, "file count/content changed");
        let second_plan =
            engine::make_plan_with_mode(std::slice::from_ref(&root), mode, &cancel, &|_| {})?;
        ensure!(
            second_plan.rows.iter().all(|r| r.status != Status::Ready),
            "second scan found additional renames"
        );
        let restored = journal::undo(&journal, &cancel, &|_| {})?;
        ensure!(before == snapshots(&root)?, "names/content not restored");
        total_renamed += renamed;
        total_restored += restored;
        modes.push(json!({"mode":mode.name(),"renamed":renamed,"restored":restored,"second_pass_unchanged":true,"samples":samples,"fixture":root}));
    }
    let result = json!({"version":engine::APP_VERSION,"engine":"MediaWiki","engine_version":ENGINE_VERSION,"rust_native":true,"modes":modes,"renamed":total_renamed,"restored":total_restored,"sha256_unchanged":true,"original_names_restored":true,"conflicts_preserved":true});
    write_json(&directory.join("verification.json"), &result)?;
    Ok(result)
}
pub fn write_json(path: &Path, value: &Value) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.sync_all()?;
    Ok(())
}

pub fn ui_fixture(directory: &Path) -> Result<(engine::Plan, Journal)> {
    fs::create_dir(directory)?;
    let root = directory.join("文件轉換範例");
    fs::create_dir_all(root.join("工作资料"))?;
    for name in [
        "软件说明.docx",
        "数据库设计.pdf",
        "合同样本.docx",
        "会议记录.txt",
        "采购明细.xlsx",
        "客户资料.csv",
        "头发保养.md",
        "岳飞传.md",
        "岳飛的软件资料.txt",
        "于谦资料.txt",
    ] {
        fs::write(root.join("工作资料").join(name), b"synthetic UI fixture")?;
    }
    fs::write(root.join("报告.txt"), b"simplified sample")?;
    fs::write(root.join("報告.txt"), b"traditional sample")?;
    let plan = engine::make_plan(&[root], &AtomicBool::new(false), &|_| {})?;
    let journal = Journal::create(&directory.join("history"), &plan)?;
    Ok((plan, journal))
}
pub fn history_for_tests(directory: &Path) -> PathBuf {
    directory.join("history")
}
