//! Native replacement and trust checks. No shell scripts or external tools.
#[cfg(unix)]
use std::fs;
use std::{io, path::Path};

#[cfg(unix)]
pub fn private_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().mode(0o700).create(path)
}

/// Security descriptor for private updater directories: owned by this user,
/// protected DACL granting only this user and SYSTEM, inherited by children.
/// Names the user's SID rather than OWNER RIGHTS: an elevated administrator
/// creates objects owned by BUILTIN\Administrators, which would lock the same
/// user's later non-elevated runs out of the directory.
#[cfg(windows)]
struct PrivateDescriptor(windows::Win32::Security::PSECURITY_DESCRIPTOR);

#[cfg(windows)]
impl PrivateDescriptor {
    fn new() -> io::Result<Self> {
        use windows::Win32::Security::{
            Authorization::{
                ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
            },
            PSECURITY_DESCRIPTOR,
        };
        let user = current_user_sid()?;
        let sddl = windows::core::HSTRING::from(format!(
            "O:{user}D:P(A;OICI;FA;;;{user})(A;OICI;FA;;;SY)"
        ));
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                &sddl,
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
        }
        .map_err(io::Error::other)?;
        Ok(Self(descriptor))
    }
}

#[cfg(windows)]
impl Drop for PrivateDescriptor {
    fn drop(&mut self) {
        use windows::Win32::Foundation::{HLOCAL, LocalFree};
        unsafe { LocalFree(Some(HLOCAL(self.0.0))) };
    }
}

#[cfg(windows)]
pub fn private_dir(path: &Path) -> io::Result<()> {
    use windows::Win32::{Security::SECURITY_ATTRIBUTES, Storage::FileSystem::CreateDirectoryW};
    let descriptor = PrivateDescriptor::new()?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0.0,
        bInheritHandle: false.into(),
    };
    unsafe { CreateDirectoryW(&wide(path), Some(&attributes)) }
        .map_err(|error| io::Error::from_raw_os_error(error.code().0 & 0xffff))
}

/// Re-apply the private owner and DACL to a directory that already exists.
/// Directories made by releases before 0.1.3 grant only OWNER RIGHTS; when an
/// elevated run created them, BUILTIN\Administrators owns them and the user's
/// non-elevated runs are denied. Rewriting the DACL also refreshes the
/// inherited ACEs of the files inside. Needs WRITE_DAC and WRITE_OWNER, which
/// the owner (or an elevated run, for Administrators-owned directories) has.
#[cfg(windows)]
pub fn secure_existing_dir(path: &Path) -> io::Result<()> {
    use windows::Win32::Security::{
        ACL,
        Authorization::{SE_FILE_OBJECT, SetNamedSecurityInfoW},
        DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner,
        OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSID,
    };
    let descriptor = PrivateDescriptor::new()?;
    unsafe {
        let mut owner = PSID::default();
        let mut defaulted = false.into();
        GetSecurityDescriptorOwner(descriptor.0, &mut owner, &mut defaulted)
            .map_err(io::Error::other)?;
        let mut present = false.into();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut dacl, &mut defaulted)
            .map_err(io::Error::other)?;
        let status = SetNamedSecurityInfoW(
            &wide(path),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION
                | DACL_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION,
            Some(owner),
            None,
            Some(dacl),
            None,
        );
        if status.is_err() {
            return Err(io::Error::from_raw_os_error(status.0 as i32));
        }
    }
    Ok(())
}

/// The current process user's SID in string form (e.g. `S-1-5-21-…`).
#[cfg(windows)]
fn current_user_sid() -> io::Result<String> {
    use windows::{
        Win32::{
            Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree},
            Security::{
                Authorization::ConvertSidToStringSidW, GetTokenInformation, TOKEN_QUERY,
                TOKEN_USER, TokenUser,
            },
            System::Threading::{GetCurrentProcess, OpenProcessToken},
        },
        core::PWSTR,
    };
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).map_err(io::Error::other)?;
        let mut needed = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
        // u64 storage keeps the TOKEN_USER header suitably aligned.
        let mut buffer = vec![0u64; (needed as usize).div_ceil(size_of::<u64>())];
        let queried = GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            needed,
            &mut needed,
        );
        let _ = CloseHandle(token);
        queried.map_err(io::Error::other)?;
        let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
        let mut text = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut text).map_err(io::Error::other)?;
        let sid = text.to_string().map_err(io::Error::other);
        LocalFree(Some(HLOCAL(text.0.cast())));
        sid
    }
}

#[cfg(unix)]
pub fn replace(target: &Path, candidate: &Path, backup: Option<&Path>) -> io::Result<()> {
    if let Some(backup) = backup {
        fs::copy(target, backup)?;
        fs::File::open(backup)?.sync_all()?;
        sync_dir(backup.parent().unwrap())?;
    }
    fs::rename(candidate, target)?;
    sync_dir(target.parent().unwrap())?;
    sync_dir(candidate.parent().unwrap())
}

#[cfg(windows)]
pub fn replace(target: &Path, candidate: &Path, backup: Option<&Path>) -> io::Result<()> {
    use windows::Win32::Storage::FileSystem::{REPLACE_FILE_FLAGS, ReplaceFileW};
    let target_w = wide(target);
    let candidate_w = wide(candidate);
    let backup_w = backup.map(wide);
    // WRITE_THROUGH is unsupported. Flush bytes before calling ReplaceFileW.
    unsafe {
        ReplaceFileW(
            &target_w,
            &candidate_w,
            backup_w
                .as_ref()
                .map_or(windows::core::PCWSTR::null(), |w| {
                    windows::core::PCWSTR(w.as_ptr())
                }),
            REPLACE_FILE_FLAGS(0),
            None,
            None,
        )
        .map_err(io::Error::other)
    }
}

#[cfg(unix)]
pub fn sync_dir(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}
#[cfg(windows)]
pub fn sync_dir(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(windows)]
pub fn wide(path: &Path) -> windows::core::HSTRING {
    use std::os::windows::ffi::OsStrExt;
    windows::core::HSTRING::from_wide(&path.as_os_str().encode_wide().collect::<Vec<_>>())
}

#[cfg(not(windows))]
pub fn verify_signature(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(windows)]
pub fn verify_signature(path: &Path) -> io::Result<()> {
    use windows::{
        Win32::{
            Foundation::HWND,
            Security::{
                Cryptography::{CERT_NAME_SIMPLE_DISPLAY_TYPE, CertGetNameStringW},
                WinTrust::*,
            },
        },
        core::PCWSTR,
    };
    let path_w = wide(path);
    let mut info = WINTRUST_FILE_INFO {
        cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: PCWSTR(path_w.as_ptr()),
        ..Default::default()
    };
    // Enumerate secondary signatures, then verify each expected signing leaf.
    for (index, expected) in ["Fara Technologies LLC", "Mike Fara"].iter().enumerate() {
        let mut signatures = WINTRUST_SIGNATURE_SETTINGS {
            cbStruct: size_of::<WINTRUST_SIGNATURE_SETTINGS>() as u32,
            dwIndex: index as u32,
            dwFlags: WSS_VERIFY_SPECIFIC | WSS_GET_SECONDARY_SIG_COUNT,
            ..Default::default()
        };
        let mut trust = WINTRUST_DATA {
            cbStruct: size_of::<WINTRUST_DATA>() as u32,
            dwUIChoice: WTD_UI_NONE,
            dwUnionChoice: WTD_CHOICE_FILE,
            Anonymous: WINTRUST_DATA_0 { pFile: &mut info },
            dwStateAction: WTD_STATEACTION_VERIFY,
            dwProvFlags: WTD_REVOCATION_CHECK_CHAIN_EXCLUDE_ROOT,
            pSignatureSettings: &mut signatures,
            ..Default::default()
        };
        let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
        let verified = unsafe {
            WinVerifyTrust(
                HWND::default(),
                &mut action,
                (&mut trust as *mut WINTRUST_DATA).cast(),
            )
        };
        let mut publisher = String::new();
        let matches = unsafe {
            let data = WTHelperProvDataFromStateData(trust.hWVTStateData);
            let signer = if data.is_null() {
                std::ptr::null_mut()
            } else {
                WTHelperGetProvSignerFromChain(data, 0, false, 0)
            };
            if verified != 0
                || signatures.cSecondarySigs != 1
                || signer.is_null()
                || (*signer).csCertChain == 0
                || (*signer).pasCertChain.is_null()
            {
                false
            } else {
                let cert = (*(*signer).pasCertChain).pCert;
                let mut name = [0u16; 512];
                let len = CertGetNameStringW(
                    cert,
                    CERT_NAME_SIMPLE_DISPLAY_TYPE,
                    0,
                    None,
                    Some(&mut name),
                );
                if len > 1 && len as usize <= name.len() {
                    publisher = String::from_utf16_lossy(&name[..len as usize - 1]);
                }
                publisher == *expected
            }
        };
        trust.dwStateAction = WTD_STATEACTION_CLOSE;
        unsafe {
            WinVerifyTrust(
                HWND::default(),
                &mut action,
                (&mut trust as *mut WINTRUST_DATA).cast(),
            );
        }
        if !matches {
            return Err(io::Error::other(format!(
                "Windows signature {index} failed verification (status {verified:08x}, secondary signatures {}, publisher {publisher:?})",
                signatures.cSecondarySigs
            )));
        }
    }
    Ok(())
}
