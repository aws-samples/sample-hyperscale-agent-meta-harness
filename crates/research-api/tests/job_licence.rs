//! Licence evidence of a repository: a pure detector over the licence
//! files and the declared fields of a repository's root. The result is evidence, never clearance: `NOASSERTION` when unsure.

#![allow(clippy::unwrap_used)]

mod job_support;

use job_support::{MIT_BODY, sha256_hex};
use research_api::licence::{
    LicenceCandidate, MAX_SPDX_BYTES, detect, is_candidate_name, select_candidates, signature_ids,
};

fn cand(path: &str, text: &str) -> LicenceCandidate {
    LicenceCandidate {
        path: path.into(),
        bytes: text.as_bytes().to_vec(),
    }
}

fn gnu(title: &str, version_line: &str, later: bool) -> String {
    let mut s = format!(
        "{title}\n{version_line}\n\nEveryone is permitted to copy and distribute verbatim copies of this license document.\n\nTERMS AND CONDITIONS\n"
    );
    if later {
        s.push_str("\nThis program is free software: you can redistribute it and/or modify it under the terms of the license as published by the Free Software Foundation, either version X of the License, or (at your option) any later version.\n");
    } else {
        s.push_str("\nThis program is free software: you can redistribute it and/or modify it under the terms of the license as published by the Free Software Foundation, version X of the License only.\n");
    }
    s
}

/// One canonical short text per SPDX id the detector can answer.
fn canonical() -> Vec<(&'static str, String)> {
    let bsd2 = "Copyright (c) 2020, Example\nAll rights reserved.\n\nRedistribution and use in source and binary forms, with or without modification, are permitted provided that the following conditions are met:\n\n1. Redistributions of source code must retain the above copyright notice, this list of conditions and the following disclaimer.\n\n2. Redistributions in binary form must reproduce the above copyright notice, this list of conditions and the following disclaimer in the documentation and/or other materials provided with the distribution.\n\nTHIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS \"AS IS\".\n";
    let bsd3 = bsd2.replace(
        "\nTHIS SOFTWARE",
        "\n3. Neither the name of the copyright holder nor the names of its contributors may be used to endorse or promote products derived from this software without specific prior written permission.\n\nTHIS SOFTWARE",
    );
    vec![
        ("MIT", MIT_BODY.to_string()),
        ("Apache-2.0", "                                 Apache License\n                           Version 2.0, January 2004\n                        http://www.apache.org/licenses/\n\n   TERMS AND CONDITIONS FOR USE, REPRODUCTION, AND DISTRIBUTION\n\n   1. Definitions.\n".to_string()),
        ("BSD-2-Clause", bsd2.to_string()),
        ("BSD-3-Clause", bsd3),
        ("ISC", "ISC License\n\nCopyright (c) 2021, Example\n\nPermission to use, copy, modify, and/or distribute this software for any purpose with or without fee is hereby granted, provided that the above copyright notice and this permission notice appear in all copies.\n\nTHE SOFTWARE IS PROVIDED \"AS IS\" AND THE AUTHOR DISCLAIMS ALL WARRANTIES.\n".to_string()),
        ("0BSD", "Zero-Clause BSD\n=============\n\nPermission to use, copy, modify, and/or distribute this software for any purpose with or without fee is hereby granted.\n\nTHE SOFTWARE IS PROVIDED \"AS IS\" AND THE AUTHOR DISCLAIMS ALL WARRANTIES.\n".to_string()),
        ("Unlicense", "This is free and unencumbered software released into the public domain.\n\nAnyone is free to copy, modify, publish, use, compile, sell, or distribute this software, either in source code form or as a compiled binary, for any purpose, commercial or non-commercial.\n".to_string()),
        ("MPL-2.0", "Mozilla Public License Version 2.0\n==================================\n\n1. Definitions\n--------------\n".to_string()),
        ("GPL-3.0-or-later", gnu("GNU GENERAL PUBLIC LICENSE", "Version 3, 29 June 2007", true)),
        ("GPL-3.0-only", gnu("GNU GENERAL PUBLIC LICENSE", "Version 3, 29 June 2007", false)),
        ("GPL-2.0-or-later", gnu("GNU GENERAL PUBLIC LICENSE", "Version 2, June 1991", true)),
        ("GPL-2.0-only", gnu("GNU GENERAL PUBLIC LICENSE", "Version 2, June 1991", false)),
        ("LGPL-2.1-or-later", gnu("GNU LESSER GENERAL PUBLIC LICENSE", "Version 2.1, February 1999", true)),
        ("LGPL-2.1-only", gnu("GNU LESSER GENERAL PUBLIC LICENSE", "Version 2.1, February 1999", false)),
        ("LGPL-3.0-or-later", gnu("GNU LESSER GENERAL PUBLIC LICENSE", "Version 3, 29 June 2007", true)),
        ("LGPL-3.0-only", gnu("GNU LESSER GENERAL PUBLIC LICENSE", "Version 3, 29 June 2007", false)),
        ("AGPL-3.0-or-later", gnu("GNU AFFERO GENERAL PUBLIC LICENSE", "Version 3, 19 November 2007", true)),
        ("AGPL-3.0-only", gnu("GNU AFFERO GENERAL PUBLIC LICENSE", "Version 3, 19 November 2007", false)),
        ("CC0-1.0", "Creative Commons Legal Code\n\nCC0 1.0 Universal\n\nCREATIVE COMMONS CORPORATION IS NOT A LAW FIRM AND DOES NOT PROVIDE LEGAL SERVICES.\n".to_string()),
        ("Zlib", "zlib License\n\nThis software is provided 'as-is', without any express or implied warranty. In no event will the authors be held liable for any damages arising from the use of this software.\n\n1. The origin of this software must not be misrepresented; you must not claim that you wrote the original software.\n2. Altered source versions must be plainly marked as such, and must not be misrepresented as being the original software.\n".to_string()),
        ("BSL-1.0", "Boost Software License - Version 1.0 - August 17th, 2003\n\nPermission is hereby granted, free of charge, to any person or organization obtaining a copy of the software and accompanying documentation covered by this license (the \"Software\") to use, reproduce, display, distribute, execute, and transmit the Software.\n".to_string()),
    ]
}

fn base_id(id: &str) -> &str {
    id.strip_suffix("-only")
        .or_else(|| id.strip_suffix("-or-later"))
        .unwrap_or(id)
}

// ---- the signature table ------------------------------------------------------------------------------------------------------------

#[test]
fn every_signature_has_a_canonical_text_that_it_recognises() {
    for (want, text) in canonical() {
        let e = detect(&[cand("LICENSE", &text)]);
        assert_eq!(e.spdx, want, "text of {want}");
        assert_eq!(e.source, "file:LICENSE");
        assert_eq!(
            e.evidence_sha256.as_deref(),
            Some(sha256_hex(text.as_bytes()).as_str())
        );
    }
}

#[test]
fn every_id_in_the_signature_table_is_covered_by_a_test_text() {
    let covered: std::collections::BTreeSet<&str> =
        canonical().iter().map(|(id, _)| base_id(id)).collect();
    let table = signature_ids();
    assert_eq!(table.len(), 16, "{table:?}");
    for id in table {
        assert!(covered.contains(id), "no canonical text for signature {id}");
    }
}

#[test]
fn near_neighbours_are_told_apart() {
    let c = canonical();
    let text = |id: &str| c.iter().find(|(i, _)| *i == id).unwrap().1.clone();
    // BSD-2 has no third clause; ISC asks that the notice appear; the GNU texts carry their own title.
    assert_eq!(
        detect(&[cand("COPYING", &text("BSD-2-Clause"))]).spdx,
        "BSD-2-Clause"
    );
    assert_eq!(
        detect(&[cand("COPYING", &text("BSD-3-Clause"))]).spdx,
        "BSD-3-Clause"
    );
    assert_eq!(detect(&[cand("COPYING", &text("ISC"))]).spdx, "ISC");
    assert_eq!(detect(&[cand("COPYING", &text("0BSD"))]).spdx, "0BSD");
    // An LGPL text names the GPL it builds on and is still the LGPL.
    let lgpl = format!(
        "{}\nThis version of the GNU Lesser General Public License incorporates the terms and conditions of version 3 of the GNU General Public License.\n",
        text("LGPL-3.0-or-later")
    );
    assert_eq!(
        detect(&[cand("COPYING.LESSER", &lgpl)]).spdx,
        "LGPL-3.0-or-later"
    );
}

#[test]
fn normalisation_ignores_line_endings_copyright_lines_bom_and_indentation() {
    let crlf = MIT_BODY.replace('\n', "\r\n");
    assert_eq!(detect(&[cand("LICENSE", &crlf)]).spdx, "MIT");
    let other_year = MIT_BODY.replace(
        "Copyright (c) 2019 Example Author",
        "Copyright 2024-2026 Somebody Else <a@b.example>\nCopyright (C) Other",
    );
    assert_eq!(detect(&[cand("LICENSE", &other_year)]).spdx, "MIT");
    let indented: String = MIT_BODY.lines().map(|l| format!("    {l}\n")).collect();
    assert_eq!(detect(&[cand("LICENSE", &indented)]).spdx, "MIT");
    let mut with_bom = vec![0xEF, 0xBB, 0xBF];
    with_bom.extend_from_slice(MIT_BODY.as_bytes());
    let e = detect(&[LicenceCandidate {
        path: "LICENSE".into(),
        bytes: with_bom.clone(),
    }]);
    assert_eq!(e.spdx, "MIT");
    assert_eq!(
        e.evidence_sha256.as_deref(),
        Some(sha256_hex(&with_bom).as_str()),
        "the hash is of the file as it is"
    );
    let mut lossy = MIT_BODY.as_bytes().to_vec();
    lossy.extend_from_slice(&[0xff, 0xfe, b'\n']);
    assert_eq!(
        detect(&[LicenceCandidate {
            path: "LICENSE".into(),
            bytes: lossy
        }])
        .spdx,
        "MIT"
    );
}

// ---- the rules for the result -----------------------------------------------------------------------------------------------------------

#[test]
fn a_lower_case_licence_file_works_and_an_unknown_text_is_noassertion() {
    let e = detect(&[cand("license", MIT_BODY)]);
    assert_eq!(
        (e.spdx.as_str(), e.source.as_str()),
        ("MIT", "file:license")
    );
    let unknown = "All rights reserved. You may not do anything with this.\n";
    let e = detect(&[cand("LICENSE", unknown)]);
    assert_eq!(
        (e.spdx.as_str(), e.source.as_str()),
        ("NOASSERTION", "file:LICENSE")
    );
    assert_eq!(
        e.evidence_sha256.as_deref(),
        Some(sha256_hex(unknown.as_bytes()).as_str())
    );
    let e = detect(&[]);
    assert_eq!(
        (
            e.spdx.as_str(),
            e.source.as_str(),
            e.evidence_sha256.clone()
        ),
        ("NOASSERTION", "none", None)
    );
    let e = detect(&[cand("package.json", r#"{"name":"x","version":"1.0.0"}"#)]);
    assert_eq!(
        (e.spdx.as_str(), e.source.as_str()),
        ("NOASSERTION", "none")
    );
}

#[test]
fn a_declared_field_answers_only_when_there_is_no_licence_file() {
    let pkg = |body: &str| detect(&[cand("package.json", body)]);
    let e = pkg(r#"{"name":"x","license":"ISC"}"#);
    assert_eq!(
        (e.spdx.as_str(), e.source.as_str()),
        ("ISC", "declared:package.json")
    );
    assert_eq!(
        e.evidence_sha256.as_deref(),
        Some(sha256_hex(br#"{"name":"x","license":"ISC"}"#).as_str())
    );
    assert_eq!(
        pkg(r#"{"license":{"type":"MIT","url":"https://example.org/mit"}}"#).spdx,
        "MIT"
    );
    assert_eq!(
        pkg(r#"{"licenses":[{"type":"MIT"},{"type":"Apache-2.0"}]}"#).spdx,
        "MIT OR Apache-2.0"
    );
    assert_eq!(
        pkg(r#"{"license":"SEE LICENSE IN LICENSE"}"#).spdx,
        "SEE LICENSE IN LICENSE"
    );
    assert_eq!(pkg("not json at all").spdx, "NOASSERTION");

    let cargo = "[package]\nname = \"x\"\nversion = \"0.1.0\"\nlicense = \"MIT OR Apache-2.0\"\n\n[dependencies]\nfoo = \"1\"\n";
    let e = detect(&[cand("Cargo.toml", cargo)]);
    assert_eq!(
        (e.spdx.as_str(), e.source.as_str()),
        ("MIT OR Apache-2.0", "declared:Cargo.toml")
    );
    let other_table = "[package]\nname = \"x\"\n\n[dependencies.foo]\nlicense = \"GPL-3.0\"\n";
    assert_eq!(
        detect(&[cand("Cargo.toml", other_table)]).spdx,
        "NOASSERTION"
    );
    let workspace = "[workspace.package]\nlicense = 'MIT'\n";
    assert_eq!(detect(&[cand("Cargo.toml", workspace)]).spdx, "MIT");

    assert_eq!(
        detect(&[cand(
            "pyproject.toml",
            "[project]\nname = \"x\"\nlicense = \"BSD-3-Clause\"\n"
        )])
        .spdx,
        "BSD-3-Clause"
    );
    assert_eq!(
        detect(&[cand(
            "pyproject.toml",
            "[project]\nlicense = { text = \"MIT\" }\n"
        )])
        .spdx,
        "MIT"
    );
    assert_eq!(
        detect(&[cand("pyproject.toml", "[tool.poetry]\nlicense = \"MIT\"\n")]).spdx,
        "MIT"
    );
    assert_eq!(
        detect(&[cand("pyproject.toml", "[tool.black]\nlicense = \"MIT\"\n")]).spdx,
        "NOASSERTION"
    );
}

#[test]
fn a_declared_text_over_128_bytes_or_with_control_characters_is_not_used() {
    let long = format!(r#"{{"license":"{}"}}"#, "A".repeat(129));
    assert_eq!(detect(&[cand("package.json", &long)]).spdx, "NOASSERTION");
    let ok = format!(r#"{{"license":"{}"}}"#, "A".repeat(MAX_SPDX_BYTES));
    assert_eq!(
        detect(&[cand("package.json", &ok)]).spdx.len(),
        MAX_SPDX_BYTES
    );
    assert_eq!(
        detect(&[cand("package.json", "{\"license\":\"MIT\\u0007\"}")]).spdx,
        "NOASSERTION"
    );
    assert_eq!(
        detect(&[cand("package.json", r#"{"license":"  MIT  "}"#)]).spdx,
        "MIT",
        "trimmed"
    );
}

#[test]
fn several_licence_files_are_joined_with_and_unless_a_declaration_covers_them() {
    let c = canonical();
    let text = |id: &str| c.iter().find(|(i, _)| *i == id).unwrap().1.clone();
    let files = [
        cand("LICENSE-MIT", &text("MIT")),
        cand("LICENSE-APACHE", &text("Apache-2.0")),
    ];
    let e = detect(&files);
    assert_eq!(
        e.spdx, "Apache-2.0 AND MIT",
        "path order, AND because nothing says OR"
    );
    assert_eq!(e.source, "file:LICENSE-APACHE,LICENSE-MIT");
    let composite = sha256_hex(
        format!(
            "{}  LICENSE-APACHE\n{}  LICENSE-MIT\n",
            sha256_hex(text("Apache-2.0").as_bytes()),
            sha256_hex(text("MIT").as_bytes())
        )
        .as_bytes(),
    );
    assert_eq!(
        e.evidence_sha256.as_deref(),
        Some(composite.as_str()),
        "the hash of the sorted `<sha256>  <path>` lines of the recognised files"
    );

    let mut with_decl = files.to_vec();
    with_decl.push(cand(
        "Cargo.toml",
        "[package]\nlicense = \"MIT OR Apache-2.0\"\n",
    ));
    let e = detect(&with_decl);
    assert_eq!(
        (e.spdx.as_str(), e.source.as_str()),
        ("MIT OR Apache-2.0", "declared:Cargo.toml")
    );
    assert_eq!(e.evidence_sha256.as_deref(), Some(composite.as_str()));

    let mut not_covering = files.to_vec();
    not_covering.push(cand("Cargo.toml", "[package]\nlicense = \"MIT\"\n"));
    assert_eq!(detect(&not_covering).spdx, "Apache-2.0 AND MIT");
    let mut parens = files.to_vec();
    parens.push(cand("package.json", r#"{"license":"(MIT OR Apache-2.0)"}"#));
    assert_eq!(detect(&parens).spdx, "(MIT OR Apache-2.0)");
}

#[test]
fn two_files_with_the_same_licence_are_one_licence_and_a_recognised_file_beats_an_unknown_one() {
    let e = detect(&[cand("LICENSE", MIT_BODY), cand("LICENSE.md", MIT_BODY)]);
    assert_eq!(
        (e.spdx.as_str(), e.source.as_str()),
        ("MIT", "file:LICENSE")
    );
    let e = detect(&[
        cand("LICENSE", MIT_BODY),
        cand("LICENSE-THIRD-PARTY", "A long list of other notices.\n"),
    ]);
    assert_eq!(
        (e.spdx.as_str(), e.source.as_str()),
        ("MIT", "file:LICENSE")
    );
}

#[test]
fn a_gnu_text_cannot_say_only_or_later_so_a_declaration_of_the_same_licence_refines_it() {
    let c = canonical();
    let text = |id: &str| c.iter().find(|(i, _)| *i == id).unwrap().1.clone();
    let gpl = text("GPL-3.0-or-later");
    let e = detect(&[
        cand("COPYING", &gpl),
        cand("package.json", r#"{"license":"GPL-3.0-only"}"#),
    ]);
    assert_eq!(
        (e.spdx.as_str(), e.source.as_str()),
        ("GPL-3.0-only", "declared:package.json")
    );
    assert_eq!(
        e.evidence_sha256.as_deref(),
        Some(sha256_hex(gpl.as_bytes()).as_str()),
        "the evidence stays the licence file"
    );
    // A declaration of another licence does not override the file.
    let e = detect(&[
        cand("COPYING", &gpl),
        cand("package.json", r#"{"license":"MIT"}"#),
    ]);
    assert_eq!(
        (e.spdx.as_str(), e.source.as_str()),
        ("GPL-3.0-or-later", "file:COPYING")
    );
    // A permissive licence is never refined.
    let e = detect(&[
        cand("LICENSE", MIT_BODY),
        cand("package.json", r#"{"license":"MIT-0"}"#),
    ]);
    assert_eq!(e.spdx, "MIT");
}

// ---- choosing the candidates -------------------------------------------------------------------------------------------------------------

#[test]
fn candidate_names_are_licence_files_and_declaration_files_at_the_root() {
    for yes in [
        "LICENSE",
        "license",
        "License.md",
        "LICENCE",
        "COPYING",
        "COPYING.LESSER",
        "UNLICENSE",
        "LICENSE-MIT",
        "LICENSE_APACHE",
        "LICENSE.txt",
        "LICENSE-APACHE.md",
        "package.json",
        "Cargo.toml",
        "pyproject.toml",
    ] {
        assert!(is_candidate_name(yes), "{yes}");
    }
    for no in [
        "src/LICENSE",
        "docs/license.md",
        "licenses/MIT.txt",
        "LICENSES",
        "license-checker.js",
        "licensed.rb",
        "readme.md",
        "package-lock.json",
        "Cargo.lock",
        "sub/package.json",
        "",
        "notice",
    ] {
        assert!(!is_candidate_name(no), "{no}");
    }
}

#[test]
fn select_candidates_takes_root_files_in_path_order() {
    let mit = MIT_BODY.as_bytes();
    let files: Vec<(&str, &[u8])> = vec![
        ("src/main.rs", b"fn main() {}"),
        ("package.json", b"{}"),
        ("LICENSE", mit),
        ("docs/LICENSE", b"other"),
        ("Cargo.toml", b"[package]"),
    ];
    let got = select_candidates(files);
    let paths: Vec<&str> = got.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(paths, ["Cargo.toml", "LICENSE", "package.json"]);
    assert_eq!(got[1].bytes, mit);
}
