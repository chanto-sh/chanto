use super::*;

#[test]
fn flavors_and_powershell_quoting() {
    assert_eq!(flavor_of("/bin/bash"), Flavor::Posix);
    assert_eq!(
        flavor_of(r"C:\Program Files\Git\bin\bash.exe"),
        Flavor::Posix
    );
    assert_eq!(
        flavor_of(r"C:\Program Files\PowerShell\7\pwsh.exe"),
        Flavor::PowerShell
    );
    assert_eq!(flavor_of("powershell.exe"), Flavor::PowerShell);
    assert_eq!(flavor_of(r"C:\Windows\system32\CMD.EXE"), Flavor::Cmd);
    assert_eq!(powershell_quote("a $(b) %c%.rs"), "'a $(b) %c%.rs'");
    assert_eq!(
        powershell_quote("it's\u{2019}.rs"),
        "'it''s\u{2019}\u{2019}.rs'"
    );
}
