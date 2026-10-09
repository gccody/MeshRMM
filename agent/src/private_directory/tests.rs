use super::test_support::*;
use super::*;
use windows::Win32::Security::Authorization::ConvertStringSidToSidW;

const FOREIGN_OWNER: &str = "O:BUD:(A;OICI;FA;;;BU)(A;OICI;FA;;;BA)";

#[test]
fn takes_over_administrator_directory_and_resets_planted_contents() {
    if !elevated() {
        return;
    }
    let root = scratch("takeover");
    let credential = root.join("agent.json");
    let updates = root.join("updates");
    let helper = updates.join("update-helper.exe");
    std::fs::write(&credential, b"{}").unwrap();
    std::fs::create_dir(&updates).unwrap();
    std::fs::write(&helper, b"MZ").unwrap();
    // Everyone was granted access explicitly, and the planted entries are owned by Users
    // and deny Administrators, as an account racing an older installer could leave them.
    set_sddl(
        &root,
        "O:BAD:(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;WD)",
    );
    set_sddl(&helper, "O:BUD:P(D;;FA;;;BA)(A;;FA;;;BU)");
    set_sddl(&updates, "O:BUD:P(D;OICI;FA;;;BA)(A;OICI;FA;;;BU)");
    set_sddl(&credential, "O:BUD:P(D;;FA;;;BA)(A;;FA;;;BU)");

    secure(&root).unwrap();
    assert_eq!(sddl_of(&root), PRIVATE_DIRECTORY_SDDL);
    secure_contents(&root).unwrap();
    assert_eq!(sddl_of(&credential), INHERITED_FILE_SDDL);
    assert_eq!(sddl_of(&updates), INHERITED_DIRECTORY_SDDL);
    assert_eq!(sddl_of(&helper), INHERITED_FILE_SDDL);
    // Securing an already private directory is idempotent.
    secure(&updates).unwrap();
    assert_eq!(sddl_of(&updates), PRIVATE_DIRECTORY_SDDL);
    remove(&root);
}

fn is_administrator_sid(sid: &str) -> bool {
    let sid = wide(OsStr::new(sid));
    let mut parsed = PSID::default();
    unsafe { ConvertStringSidToSidW(PCWSTR(sid.as_ptr()), &mut parsed) }.unwrap();
    let member = is_administrator(parsed);
    unsafe {
        LocalFree(Some(HLOCAL(parsed.0)));
    }
    member
}

#[test]
fn recognizes_administrators_through_group_membership() {
    // Everyone, Users, and an account no domain issued.
    assert!(!is_administrator_sid("S-1-1-0"));
    assert!(!is_administrator_sid("S-1-5-32-545"));
    assert!(!is_administrator_sid("S-1-5-21-1-2-3-4242"));
    if !elevated() {
        return;
    }
    let token = process_token(TOKEN_QUERY).unwrap();
    let mut length = 0;
    let _ = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut length) };
    let mut buffer = vec![0_u64; (length as usize).div_ceil(8)];
    unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            length,
            &mut length,
        )
    }
    .unwrap();
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    assert!(is_administrator(user.User.Sid));
}

/// The SYSTEM service securing a directory an administrator created under the "object creator"
/// owner policy. Run elevated as another account, such as SYSTEM, with
/// `MESHRMM_TEST_ADMINISTRATOR_SID` naming an administrator account.
#[test]
#[ignore = "needs a second administrator account"]
fn takes_over_a_directory_another_administrator_owns() {
    let administrator = std::env::var("MESHRMM_TEST_ADMINISTRATOR_SID").unwrap();
    assert!(elevated());
    let root = scratch("administrator-owned");
    set_sddl(&root, &format!("O:{administrator}D:(A;OICI;FA;;;BA)"));
    assert!(sddl_of(&root).starts_with(&format!("O:{administrator}")));
    secure(&root).unwrap();
    assert_eq!(sddl_of(&root), PRIVATE_DIRECTORY_SDDL);
    remove(&root);
}

#[test]
fn refuses_directory_owned_by_another_account() {
    if !elevated() {
        return;
    }
    let root = scratch("foreign");
    set_sddl(&root, FOREIGN_OWNER);
    let before = sddl_of(&root);
    assert!(before.starts_with("O:BU"), "{before}");
    let error = secure(&root).unwrap_err();
    assert!(
        error.downcast_ref::<UntrustedOwner>().is_some(),
        "{error:#}"
    );
    assert_eq!(sddl_of(&root), before);
    set_sddl(&root, PRIVATE_DIRECTORY_SDDL);
    remove(&root);
}

#[test]
fn creates_missing_directories_and_new_ones_must_not_exist() {
    if !elevated() {
        return;
    }
    let root = scratch("create");
    let missing = root.join("Agent");
    secure(&missing).unwrap();
    assert_eq!(sddl_of(&missing), PRIVATE_DIRECTORY_SDDL);
    assert!(create_new(&missing).is_err());
    let fresh = root.join("staging");
    create_new(&fresh).unwrap();
    assert_eq!(sddl_of(&fresh), PRIVATE_DIRECTORY_SDDL);
    remove(&root);
}

#[test]
fn reads_legacy_files_only_when_administrators_control_them() {
    if !elevated() {
        return;
    }
    let root = scratch("legacy");
    let product = root.join("PulseRMM");
    let directory = product.join("Agent");
    let file = directory.join("agent.json");
    assert!(read_protected_file(&file).unwrap().is_none());
    std::fs::create_dir_all(&directory).unwrap();
    assert!(read_protected_file(&file).unwrap().is_none());
    std::fs::write(&file, b"{}").unwrap();
    // As a legacy installation leaves them: the product folder keeps what ProgramData passes
    // down, including the Users grant to create entries, and icacls protected the Agent folder.
    const PRODUCT: &str =
        "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;CI;0x116;;;BU)(A;OICIIO;FA;;;CO)";
    const DIRECTORY: &str = "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";
    const FILE: &str = "O:BAD:(A;ID;FA;;;SY)(A;ID;FA;;;BA)(A;;FR;;;BU)";
    set_sddl(&product, PRODUCT);
    set_sddl(&directory, DIRECTORY);
    set_sddl(&file, FILE);
    assert_eq!(read_protected_file(&file).unwrap().unwrap(), b"{}");

    let untrusted = |path: &Path, descriptor: &str, original: &str| {
        set_sddl(path, descriptor);
        let error = read_protected_file(&file).unwrap_err();
        let reason = error
            .downcast_ref::<UntrustedPath>()
            .unwrap_or_else(|| panic!("{error:#}"));
        assert_eq!(reason.path, path);
        set_sddl(path, original);
    };
    untrusted(&file, "O:BUD:(A;;FA;;;SY)(A;;FA;;;BA)", FILE);
    untrusted(
        &file,
        "O:BAD:(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x12019f;;;BU)",
        FILE,
    );
    untrusted(&file, "O:BAD:NO_ACCESS_CONTROL", FILE);
    untrusted(
        &directory,
        "O:BUD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)",
        DIRECTORY,
    );
    untrusted(
        &directory,
        "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;CI;0x116;;;BU)",
        DIRECTORY,
    );
    untrusted(&product, "O:BUD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)", PRODUCT);
    untrusted(
        &product,
        "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;;0x40;;;BU)",
        PRODUCT,
    );
    assert_eq!(read_protected_file(&file).unwrap().unwrap(), b"{}");

    // A planted junction is reported as untrusted without reading through it.
    std::fs::remove_file(&file).unwrap();
    std::fs::remove_dir(&directory).unwrap();
    let target = root.join("target");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("agent.json"), b"{}").unwrap();
    let status = std::process::Command::new("cmd.exe")
        .args(["/D", "/C", "mklink", "/J"])
        .arg(&directory)
        .arg(&target)
        .output()
        .unwrap()
        .status;
    assert!(status.success());
    let error = read_protected_file(&file).unwrap_err();
    let reason = error.downcast_ref::<UntrustedPath>().unwrap();
    assert_eq!(reason.path, directory);
    remove(&root);
}

#[test]
fn refuses_junctions_without_changing_their_targets() {
    if !elevated() {
        return;
    }
    let root = scratch("junction");
    let target = root.join("target");
    std::fs::create_dir(&target).unwrap();
    let original = sddl_of(&target);
    let junction = |link: &Path| {
        let status = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(link)
            .arg(&target)
            .output()
            .unwrap()
            .status;
        assert!(status.success());
    };

    let link = root.join("link");
    junction(&link);
    let error = secure(&link).unwrap_err();
    assert!(format!("{error:#}").contains("reparse point"), "{error:#}");

    let private = root.join("private");
    secure(&private).unwrap();
    junction(&private.join("identity"));
    let error = secure_contents(&private).unwrap_err();
    assert!(format!("{error:#}").contains("reparse point"), "{error:#}");
    assert_eq!(sddl_of(&target), original);
    remove(&root);
}

#[test]
fn installed_files_inherit_their_new_directory_security() {
    if !elevated() {
        return;
    }
    // Like Program Files, the install directory lets Users read and execute its files.
    let install = scratch("install");
    set_sddl(
        &install,
        "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)",
    );
    let agent = install.join("meshrmm-agent.exe");
    std::fs::write(&agent, b"MZ").unwrap();
    // A file renamed out of the private update directory keeps what it inherited there.
    set_sddl(&agent, INHERITED_FILE_SDDL);
    inherit_parent_security(&agent).unwrap();
    let installed = sddl_of(&agent);
    assert!(installed.contains("(A;ID;0x1200a9;;;BU)"), "{installed}");
    assert!(!installed.contains("D:P"), "{installed}");

    let private = scratch("private-install");
    set_sddl(&private, PRIVATE_DIRECTORY_SDDL);
    let hidden = private.join("meshrmm-agent.exe");
    std::fs::write(&hidden, b"MZ").unwrap();
    let error = inherit_parent_security(&hidden).unwrap_err();
    assert!(format!("{error:#}").contains("Users read"), "{error:#}");
    remove(&install);
    remove(&private);
}
