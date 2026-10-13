// wwrpc - Wuthering Waves Discord Rich Presence for Linux.
// Copyright (C) 2026 yiesko
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version. See COPYING for details.

//! Playable-character icon keys for `--character`.
//!
//! Every character carries two art keys, and which one applies depends on
//! the slot. `asset` is a Discord art-asset key, so it renders only after
//! that PNG is uploaded to the application under the same lowercase name;
//! `file` is the upstream stem, and [`portrait_url`] turns it into the
//! external `https://` URL that renders in any slot with no upload at all.
//!
//! Portraits come from `ryanbenson/wuthering-waves-assets`, `images/*.png`,
//! PascalCase stems (`Carlotta.png`, `XiangliYao.png`, Rover variants
//! ending in `Male`/`Female`). They are never vendored here: the upstream
//! repo carries no license and the art belongs to Kuro Games. A new
//! character is one line in the `CHARACTERS` table below.

/// One selectable character: match key, Discord asset key, hover text,
/// upstream file stem.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Character {
    /// Normalized match key (lowercase alphanumeric, e.g. `xiangliyao`).
    pub key: &'static str,
    /// Discord art-asset key: file stem lowercased (e.g. `cartethyia`).
    pub asset: &'static str,
    /// Hover text (e.g. `Xiangli Yao`).
    pub display: &'static str,
    /// Exact upstream file stem for external URLs, e.g. `XiangliYao`
    /// (case-sensitive on CDNs; upstream renames happen, e.g. Rover
    /// Electro variants).
    pub file: &'static str,
}

/// `(key, asset, display, file)`, alphabetical by key. Generated from
/// the upstream `images/` listing; Rover variants keep their suffix.
const CHARACTERS: &[(&str, &str, &str, &str)] = &[
    ("aalto", "aalto", "Aalto", "Aalto"),
    ("aemeath", "aemeath", "Aemeath", "Aemeath"),
    ("augusta", "augusta", "Augusta", "Augusta"),
    ("baizhi", "baizhi", "Baizhi", "Baizhi"),
    ("brant", "brant", "Brant", "Brant"),
    ("buling", "buling", "Buling", "Buling"),
    ("calcharo", "calcharo", "Calcharo", "Calcharo"),
    ("camellya", "camellya", "Camellya", "Camellya"),
    ("cantarella", "cantarella", "Cantarella", "Cantarella"),
    ("carlotta", "carlotta", "Carlotta", "Carlotta"),
    ("cartethyia", "cartethyia", "Cartethyia", "Cartethyia"),
    ("changli", "changli", "Changli", "Changli"),
    ("chisa", "chisa", "Chisa", "Chisa"),
    ("chixia", "chixia", "Chixia", "Chixia"),
    ("ciaccona", "ciaccona", "Ciaccona", "Ciaccona"),
    ("danjin", "danjin", "Danjin", "Danjin"),
    ("denia", "denia", "Denia", "Denia"),
    ("encore", "encore", "Encore", "Encore"),
    ("galbrena", "galbrena", "Galbrena", "Galbrena"),
    ("hiyuki", "hiyuki", "Hiyuki", "Hiyuki"),
    ("hsin", "hsin", "Hsin", "Hsin"),
    ("iuno", "iuno", "Iuno", "Iuno"),
    ("jianxin", "jianxin", "Jianxin", "Jianxin"),
    ("jingran", "jingran", "Jingran", "Jingran"),
    ("jinhsi", "jinhsi", "Jinhsi", "Jinhsi"),
    ("jiyan", "jiyan", "Jiyan", "Jiyan"),
    ("lingyang", "lingyang", "Lingyang", "Lingyang"),
    ("lucilla", "lucilla", "Lucilla", "Lucilla"),
    ("lucy", "lucy", "Lucy", "Lucy"),
    ("lumi", "lumi", "Lumi", "Lumi"),
    ("lupa", "lupa", "Lupa", "Lupa"),
    ("luukherssen", "luukherssen", "LuukHerssen", "LuukHerssen"),
    ("lynae", "lynae", "Lynae", "Lynae"),
    ("mornye", "mornye", "Mornye", "Mornye"),
    ("mortefi", "mortefi", "Mortefi", "Mortefi"),
    ("phoebe", "phoebe", "Phoebe", "Phoebe"),
    ("phrolova", "phrolova", "Phrolova", "Phrolova"),
    ("qingxiao", "qingxiao", "Qingxiao", "Qingxiao"),
    ("qiuyuan", "qiuyuan", "Qiuyuan", "Qiuyuan"),
    ("rebecca", "rebecca", "Rebecca", "Rebecca"),
    ("roccia", "roccia", "Roccia", "Roccia"),
    (
        "roveraerofemale",
        "roveraerofemale",
        "Rover (Aero, Female)",
        "RoverAeroFemale",
    ),
    (
        "roveraeromale",
        "roveraeromale",
        "Rover (Aero, Male)",
        "RoverAeroMale",
    ),
    (
        "roverelectrofemale",
        "roverelectrofemale",
        "Rover (Electro, Female)",
        "Roverelectrofemale",
    ),
    (
        "roverelectromale",
        "roverelectromale",
        "Rover (Electro, Male)",
        "Roverelectromale",
    ),
    (
        "roverhavocfemale",
        "roverhavocfemale",
        "Rover (Havoc, Female)",
        "RoverHavocFemale",
    ),
    (
        "roverhavocmale",
        "roverhavocmale",
        "Rover (Havoc, Male)",
        "RoverHavocMale",
    ),
    (
        "roverspectrofemale",
        "roverspectrofemale",
        "Rover (Spectro, Female)",
        "RoverSpectroFemale",
    ),
    (
        "roverspectromale",
        "roverspectromale",
        "Rover (Spectro, Male)",
        "RoverSpectroMale",
    ),
    ("sanhua", "sanhua", "Sanhua", "Sanhua"),
    ("shorekeeper", "shorekeeper", "Shorekeeper", "Shorekeeper"),
    ("sigrika", "sigrika", "Sigrika", "Sigrika"),
    ("suisui", "suisui", "Suisui", "Suisui"),
    ("suoming", "suoming", "Suoming", "Suoming"),
    ("taoqi", "taoqi", "Taoqi", "Taoqi"),
    ("verina", "verina", "Verina", "Verina"),
    ("xiangliyao", "xiangliyao", "Xiangli Yao", "XiangliYao"),
    ("yangyang", "yangyang", "Yangyang", "Yangyang"),
    ("yinlin", "yinlin", "Yinlin", "Yinlin"),
    ("youhu", "youhu", "Youhu", "Youhu"),
    ("yuanwu", "yuanwu", "Yuanwu", "Yuanwu"),
    ("zani", "zani", "Zani", "Zani"),
    ("zhezhi", "zhezhi", "Zhezhi", "Zhezhi"),
];

/// Base URL for portrait images (jsDelivr mirror of the upstream repo).
/// External `https://` art renders in any slot with zero uploads.
pub const PORTRAIT_BASE_URL: &str =
    "https://cdn.jsdelivr.net/gh/ryanbenson/wuthering-waves-assets@master/images";

/// Full portrait URL for a character, e.g. `.../Denia.png`.
pub fn portrait_url(character: &Character) -> String {
    format!("{PORTRAIT_BASE_URL}/{}.png", character.file)
}

/// Normalize free input for matching: lowercase alphanumeric only, so
/// `Xiangli Yao`, `xiangli-yao` and `XiangliYao` all meet `xiangliyao`.
pub fn normalize(input: &str) -> String {
    input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_lowercase()
}

/// Resolve `--character` input to a [`Character`]. `None` for blank or
/// unknown input — the caller warns and publishes without an icon.
pub fn find(input: &str) -> Option<Character> {
    let key = normalize(input);
    if key.is_empty() {
        return None;
    }
    CHARACTERS
        .iter()
        .find(|(known, _, _, _)| *known == key)
        .map(|(key, asset, display, file)| Character {
            key,
            asset,
            display,
            file,
        })
}

/// Every selectable character, alphabetical. Backs `--list-characters`.
pub fn all() -> impl Iterator<Item = Character> {
    CHARACTERS
        .iter()
        .map(|(key, asset, display, file)| Character {
            key,
            asset,
            display,
            file,
        })
}
