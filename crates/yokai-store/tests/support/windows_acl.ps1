# Test-only current-user DACL denial and permission-rule restoration on owned fixtures.
# Supplied over stdin to pinned Windows PowerShell with no profile/policy override.
& {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'
    Set-StrictMode -Version Latest
    [Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
    function Full-Path([string] $Value) {
        if ([string]::IsNullOrWhiteSpace($Value)) { throw 'Missing ACL path' }
        if ($Value.StartsWith('\\?\')) { $Value = $Value.Substring(4) }
        if ($Value -notmatch '^[A-Za-z]:\\') { throw 'ACL path must be a local absolute path' }
        return [IO.Path]::GetFullPath($Value).TrimEnd('\')
    }
    function No-Reparse([string] $Path) {
        $Current = $Path
        while ($Current) {
            $Item = Get-Item -LiteralPath $Current -Force
            if (($Item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'ACL path contains reparse entry' }
            $Parent = [IO.Directory]::GetParent($Current)
            if ($null -eq $Parent) { break }
            $Current = $Parent.FullName
        }
    }
    function Access-Sddl([string] $Path) {
        return (Get-Acl -LiteralPath $Path).GetSecurityDescriptorSddlForm([Security.AccessControl.AccessControlSections]::Access)
    }
    function Ace-Bytes($Ace) {
        $Bytes = New-Object byte[] $Ace.BinaryLength
        $Ace.GetBinaryForm($Bytes, 0)
        return [Convert]::ToBase64String($Bytes)
    }
    function Check-Control($Before, $After) {
        $OldFlags = [int]$Before.ControlFlags; $NewFlags = [int]$After.ControlFlags
        if ((($OldFlags -bxor $NewFlags) -band 0xfbff) -ne 0 -or (($OldFlags -band 0x400) -ne 0 -and ($NewFlags -band 0x400) -eq 0)) { throw 'Unexpected descriptor-control change' }
    }
    function Check-Restored([string] $Before, [string] $After) {
        $Old = [Security.AccessControl.RawSecurityDescriptor]::new($Before)
        $New = [Security.AccessControl.RawSecurityDescriptor]::new($After)
        if ($null -eq $Old.DiscretionaryAcl -or $null -eq $New.DiscretionaryAcl -or $Old.DiscretionaryAcl.Count -ne $New.DiscretionaryAcl.Count -or $Old.DiscretionaryAcl.Revision -ne $New.DiscretionaryAcl.Revision) { throw 'Original DACL layout differs' }
        for ($Index = 0; $Index -lt $Old.DiscretionaryAcl.Count; $Index++) {
            if ((Ace-Bytes $Old.DiscretionaryAcl[$Index]) -cne (Ace-Bytes $New.DiscretionaryAcl[$Index])) { throw 'Original DACL permission ACE bytes differ' }
        }
        # Set-Acl can initialize AUTO_INHERITED on these new fixtures. Preserve all
        # other control bits, including protection; record the complete descriptors.
        Check-Control $Old $New
    }
    function Write-New([string] $Path, [string] $Text) {
        $Stream = [IO.File]::Open($Path, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
        $Writer = [IO.StreamWriter]::new($Stream, [Text.UTF8Encoding]::new($false))
        try { $Writer.Write($Text); $Writer.Flush(); $Stream.Flush($true) } finally { $Writer.Dispose() }
    }
    try {
        $Base = Full-Path $env:YOKAI_TEST_DATA_DIR
        $Root = Full-Path $env:YOKAI_TEST_ACL_ROOT
        $Journal = Full-Path $env:YOKAI_TEST_ACL_JOURNAL
        $Case = $env:YOKAI_TEST_ACL_CASE
        $Operation = $env:YOKAI_TEST_ACL_OPERATION
        $Label = $env:YOKAI_TEST_ACL_LABEL
        if ($Label -cnotmatch '^[a-z-]{1,40}$') { throw 'Unexpected ACL receipt label' }
        if (-not $Root.StartsWith($Base + '\', [StringComparison]::OrdinalIgnoreCase)) { throw 'ACL root escapes test data' }
        if ($Root.Substring($Base.Length + 1) -notmatch '^windows_validation-[0-9]+-preflight-acl-[0-9]+\\[0-9a-f]{32}$') { throw 'Unexpected ACL fixture layout' }
        if ($Journal -cne ([IO.Path]::GetDirectoryName($Root) + '\acl-journal')) { throw 'Unexpected ACL journal location' }
        foreach ($Path in @($Base, $Root, $Journal)) { No-Reparse $Path }
        if (-not (Get-Item -LiteralPath $Root).PSIsContainer -or -not (Get-Item -LiteralPath $Journal).PSIsContainer) { throw 'ACL root/journal is not a directory' }
        $Sid = [Security.Principal.WindowsIdentity]::GetCurrent().User
        switch -Exact ($Case) {
            'db-read' { $Target = $Root + '\instance.db'; $Rights = [Security.AccessControl.FileSystemRights]::ReadData }
            'shm-read' { $Target = $Root + '\instance.db-shm'; $Rights = [Security.AccessControl.FileSystemRights]::ReadData }
            'directory-create' { $Target = $Root; $Rights = [Security.AccessControl.FileSystemRights]::CreateFiles -bor [Security.AccessControl.FileSystemRights]::CreateDirectories }
            default { throw 'Unexpected ACL case' }
        }
        No-Reparse $Target
        if ($Case -ne 'directory-create' -and (Get-Item -LiteralPath $Target).PSIsContainer) { throw 'ACL file target is a directory' }
        $RecordPath = $Journal + '\original.json'
        if ($Operation -ceq 'deny') {
            $Scope = [ordered]@{ '.' = (Access-Sddl $Root) }
            foreach ($Entry in Get-ChildItem -LiteralPath $Root -Force) {
                if ($Entry.PSIsContainer -or $Entry.Name -notmatch '^(instance\.(db(-wal|-shm)?|id|lock))$') { throw 'Unexpected ACL fixture entry' }
                No-Reparse $Entry.FullName
                $Scope[$Entry.Name] = Access-Sddl $Entry.FullName
            }
            $Original = Access-Sddl $Target
            $Record = [ordered]@{ Version = 2; Root = $Root; Target = $Target; Case = $Case; Sid = $Sid.Value; Dacl = $Original; Scope = $Scope; Restoration = 'ExactAceBytesAndProtection;AutoInheritedInitializationRecorded' }
            Write-New $RecordPath ($Record | ConvertTo-Json -Depth 5 -Compress)
            $Acl = Get-Acl -LiteralPath $Target
            $ExistingDenies = @($Acl.GetAccessRules($true, $false, [Security.Principal.SecurityIdentifier]) | Where-Object {
                $_.IdentityReference.Value -ceq $Sid.Value -and $_.AccessControlType -eq [Security.AccessControl.AccessControlType]::Deny
            })
            if ($ExistingDenies.Count -ne 0) { throw 'Fixture already has an explicit current-SID deny' }
            $Rule = [Security.AccessControl.FileSystemAccessRule]::new($Sid, $Rights, [Security.AccessControl.InheritanceFlags]::None, [Security.AccessControl.PropagationFlags]::None, [Security.AccessControl.AccessControlType]::Deny)
            $Acl.AddAccessRule($Rule)
            Set-Acl -LiteralPath $Target -AclObject $Acl
            $Applied = Get-Acl -LiteralPath $Target
            $DenyRules = @($Applied.GetAccessRules($true, $false, [Security.Principal.SecurityIdentifier]) | Where-Object {
                $_.IdentityReference.Value -ceq $Sid.Value -and $_.AccessControlType -eq [Security.AccessControl.AccessControlType]::Deny -and
                $_.InheritanceFlags -eq [Security.AccessControl.InheritanceFlags]::None -and $_.PropagationFlags -eq [Security.AccessControl.PropagationFlags]::None -and
                ($_.FileSystemRights -band $Rights) -eq $Rights
            })
            if ($DenyRules.Count -ne 1 -or (Access-Sddl $Target) -ceq $Original) { throw 'Minimal explicit current-SID deny not observed' }
            $AppliedScope = [ordered]@{}
            foreach ($Name in $Scope.Keys) {
                $Path = if ($Name -ceq '.') { $Root } else { $Root + '\' + $Name }
                $AppliedScope[$Name] = Access-Sddl $Path
                if ($Path -cne $Target) { Check-Restored $Scope[$Name] $AppliedScope[$Name] }
            }
            Write-New ($Journal + '\applied-sddl.txt') (Access-Sddl $Target)
            Write-New ($Journal + '\applied-scope.json') ($AppliedScope | ConvertTo-Json -Compress)
        } elseif ($Operation -ceq 'restore') {
            No-Reparse $RecordPath
            $Record = [IO.File]::ReadAllText($RecordPath, [Text.Encoding]::UTF8) | ConvertFrom-Json
            if ($Record.Version -ne 2 -or $Record.Restoration -cne 'ExactAceBytesAndProtection;AutoInheritedInitializationRecorded' -or $Record.Root -cne $Root -or $Record.Target -cne $Target -or $Record.Case -cne $Case -or $Record.Sid -cne $Sid.Value -or $Record.Dacl -notmatch '^D:') { throw 'ACL journal binding differs' }
            if ((Access-Sddl $Target) -cne $Record.Dacl) {
                $Acl = Get-Acl -LiteralPath $Target
                $Acl.SetSecurityDescriptorSddlForm($Record.Dacl, [Security.AccessControl.AccessControlSections]::Access)
                Set-Acl -LiteralPath $Target -AclObject $Acl
            }
            Check-Restored $Record.Dacl (Access-Sddl $Target)
            $RestoredScope = [ordered]@{}
            foreach ($Property in $Record.Scope.PSObject.Properties) {
                if ($Property.Name -cne '.' -and $Property.Name -notmatch '^instance\.(db(-wal|-shm)?|id|lock)$') { throw 'ACL scope path rejected' }
                $Path = if ($Property.Name -ceq '.') { $Root } else { $Root + '\' + $Property.Name }
                No-Reparse $Path
                $RestoredScope[$Property.Name] = Access-Sddl $Path
                Check-Restored $Property.Value $RestoredScope[$Property.Name]
            }
            Write-New ($Journal + '\restored-scope-' + $Label + '.json') ($RestoredScope | ConvertTo-Json -Compress)
        } else { throw 'Unexpected ACL operation' }
        [Console]::WriteLine('ACL_OPERATION ' + $Operation + ' PASS')
        exit 0
    } catch {
        [Console]::Error.WriteLine($_.Exception.Message)
        exit 1
    }
}
