//! Builds the crate for one entry of the repository's `accounts.json`, or of the file
//! `HAMH_ACCOUNTS_FILE` names:
//! `HAMH_AWS_PROFILE` names the entry by its profile, unset or empty selects the active one. The earlier
//! spellings of both variables are read until VERSION 5.50 (see [`paired_variable`]). It
//! writes the account, home Region and profile constants `target` includes, so
//! every Rust binary targets the account `scripts/lib/aws_target.py` selects for the same
//! environment. A malformed file or an unknown profile fails the build.

use std::path::PathBuf;

include!("src/accounts.rs");

/// One variable under its two spellings, with a cargo warning for the earlier one; a conflict fails the build.
fn read_variable(new_name: &str, old_name: &str) -> String {
    let (new, old) = (std::env::var_os(new_name), std::env::var_os(old_name));
    println!("cargo:rerun-if-env-changed={new_name}");
    println!("cargo:rerun-if-env-changed={old_name}");
    let (value, notice) = paired_variable(new_name, new.as_deref(), old_name, old.as_deref())
        .unwrap_or_else(|e| panic!("{e}"));
    if let Some(notice) = notice {
        println!("cargo:warning={notice}");
    }
    value
}

fn main() {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let over = read_variable(ACCOUNTS_FILE_ENV, ACCOUNTS_FILE_ENV_EARLIER);
    let file = accounts_file_path(Some(std::ffi::OsStr::new(&over)), &manifest)
        .unwrap_or_else(|e| panic!("{e}"));
    println!("cargo:rerun-if-changed={}", file.display());
    println!("cargo:rerun-if-changed=src/accounts.rs");
    let text = std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
    let entries = parse_accounts(&text).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
    let by = read_variable(ACCOUNT_SELECTOR, ACCOUNT_SELECTOR_EARLIER);
    let label = if over.is_empty() {
        "accounts.json".to_owned()
    } else {
        file.display().to_string()
    };
    let t = select_account(&entries, &by, &label).unwrap_or_else(|e| panic!("{e}"));
    let members = operator_pool_members(&t.account, t.operator.as_ref());
    let mut target = format!(
        "/// The selected entry's account: `cc:` repositories, ARNs and the caller checks.\n\
         pub const E2E_ACCOUNT: &str = {:?};\n\
         /// The selected entry's home Region.\n\
         pub const E2E_REGION: &str = {:?};\n\
         /// The selected entry's AWS CLI and SDK profile.\n\
         pub const E2E_PROFILE: &str = {:?};\n\
         /// The caller pattern of the entry's operator (`operator` of the entry; the IAM role `Admin` when it names none): the first of `OPERATOR_POOL_MEMBERS`.\n\
         pub const OPERATOR_POOL_MEMBER: &str = {:?};\n\
         /// Every caller pattern of the entry's operator: one for a role, the user ARN with and without a path for a user.\n\
         pub const OPERATOR_POOL_MEMBERS: &[&str] = &{:?};\n\
         /// The `HAMH_AWS_PROFILE` value the build used (empty: the active entry).\n\
         pub const SELECTED_BY: &str = {:?};\n\
         /// Every entry of `accounts.json` in file order: (account, Region, profile, role).\n\
         pub const ACCOUNTS: &[(&str, &str, &str, &str)] = &[\n",
        t.account, t.region, t.profile, members[0], members, by
    );
    for e in &entries {
        target.push_str(&format!(
            "    ({:?}, {:?}, {:?}, {:?}),\n",
            e.account, e.region, e.profile, e.role
        ));
    }
    target.push_str("];\n");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or_default());
    let dest = out.join("aws_target.rs");
    std::fs::write(&dest, target).unwrap_or_else(|e| panic!("{}: {e}", dest.display()));
}
