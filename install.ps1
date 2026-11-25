# ngiab-rs installer for Windows.
#
#   powershell -ExecutionPolicy ByPass -c "irm https://github.com/JoshCu/ngiab-rs/releases/latest/download/install.ps1 | iex"
#
# Configuration comes only from environment variables (no param() block: it
# does not work with `irm | iex`):
#   NGIAB_RS_VERSION         release to install, e.g. 0.2.0 (default: latest)
#   NGIAB_RS_INSTALL_DIR     default: %USERPROFILE%\.local\bin
#   NGIAB_RS_BACKENDS        docker, podman, all or none (comma separated); skips the menu
#   NGIAB_RS_SKIP_BINARY=1   only set up backends (used by `ngiab-rs backend install`)
#   NGIAB_RS_NO_MODIFY_PATH=1  don't add the install dir to the user Path
#   NGIAB_RS_YES=1           accept every prompt
#   NGIAB_RS_REPO            GitHub repository (default: JoshCu/ngiab-rs)
#   NGIAB_RS_IMAGE           image to pull (default: docker.io/joshcu/ngiab-rs:<version>)
#   NGIAB_RS_DOWNLOAD_URL    fetch release assets from this base URL instead of GitHub
#                            (mirrors and testing)
#
# Everything runs inside one script block, so `irm | iex` leaves no functions or
# preference changes behind in the user's session, and `exit` is only used when
# run with -File, so a failure never closes the user's PowerShell window.

$ngiabRsExit = & {
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'  # Invoke-WebRequest is very slow with the progress bar on 5.1

function Test-Truthy([string]$Value) {
    return @('1', 'true', 'yes', 'y', 'on') -contains "$Value".Trim().ToLowerInvariant()
}

function Write-Step([string]$Message) { Write-Host $Message }
function Write-Warn([string]$Message) { Write-Host "warning: $Message" -ForegroundColor Yellow }
function Write-Err([string]$Message) { Write-Host "error: $Message" -ForegroundColor Red }

function Get-Settings {
    $repo = if ($env:NGIAB_RS_REPO) { $env:NGIAB_RS_REPO } else { 'JoshCu/ngiab-rs' }
    $installDir = if ($env:NGIAB_RS_INSTALL_DIR) { $env:NGIAB_RS_INSTALL_DIR } else { Join-Path $env:USERPROFILE '.local\bin' }
    $version = "$($env:NGIAB_RS_VERSION)".Trim()
    if ($version.StartsWith('v')) { $version = $version.Substring(1) }
    $downloadUrl = "$($env:NGIAB_RS_DOWNLOAD_URL)".Trim().TrimEnd('/')
    $yes = Test-Truthy $env:NGIAB_RS_YES

    # Prompts only in an interactive console that isn't being fed from a pipe.
    $interactive = [Environment]::UserInteractive -and -not [Console]::IsInputRedirected
    foreach ($a in [Environment]::GetCommandLineArgs()) {
        if ($a -match '^-noni') { $interactive = $false }
    }

    return [pscustomobject]@{
        Repo         = $repo
        InstallDir   = $installDir
        Version      = $version
        Backends     = "$($env:NGIAB_RS_BACKENDS)".Trim()
        SkipBinary   = Test-Truthy $env:NGIAB_RS_SKIP_BINARY
        NoModifyPath = Test-Truthy $env:NGIAB_RS_NO_MODIFY_PATH
        Yes          = $yes
        DownloadUrl  = $downloadUrl
        Interactive  = $interactive
    }
}

# Read-Host only when interactive and NGIAB_RS_YES is not set. Returns $true for yes.
function Confirm-Step($S, [string]$Question, [bool]$Default = $true) {
    if ($S.Yes) { Write-Host "$Question [yes: NGIAB_RS_YES]"; return $true }
    if (-not $S.Interactive) {
        Write-Host "$Question [no: not interactive; set NGIAB_RS_YES=1 to accept]"
        return $false
    }
    $hint = if ($Default) { '[Y/n]' } else { '[y/N]' }
    $answer = "$(Read-Host "$Question $hint")".Trim().ToLowerInvariant()
    if ($answer -eq '') { return $Default }
    return @('y', 'yes') -contains $answer
}

# Run a native command with output going to the console. Returns $true on exit code 0.
function Invoke-Native([string]$Exe, [string[]]$Arguments) {
    $ErrorActionPreference = 'Continue'
    try {
        # Out-Host: the command's output must not become this function's return value.
        & $Exe @Arguments | Out-Host
        return ($LASTEXITCODE -eq 0)
    } catch {
        Write-Err "$Exe failed: $_"
        return $false
    }
}

# Run a native command silently (stderr included). Returns $true on exit code 0.
function Test-Native([string]$Exe, [string[]]$Arguments) {
    $ErrorActionPreference = 'Continue'
    try {
        & $Exe @Arguments 2>&1 | Out-Null
        return ($LASTEXITCODE -eq 0)
    } catch {
        return $false
    }
}

# Native command output as a string (stdout only), or $null on failure.
function Get-NativeOutput([string]$Exe, [string[]]$Arguments) {
    $ErrorActionPreference = 'Continue'
    try {
        $out = & $Exe @Arguments 2>$null
        if ($LASTEXITCODE -ne 0) { return $null }
        return (@($out) -join "`n")
    } catch {
        return $null
    }
}

function Get-Arch {
    $arch = $null
    try {
        $arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
    } catch {
        $arch = $null  # older .NET Framework: fall back to the environment below
    }
    if (-not $arch) {
        $arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
    }
    switch -Regex ("$arch".ToUpperInvariant()) {
        '^(X64|AMD64)$' { return 'x64' }
        '^ARM64$' { return 'arm64' }
        default { return "$arch" }
    }
}

function Get-ReleaseBase($S) {
    if ($S.DownloadUrl) { return $S.DownloadUrl }
    if ($S.Version) { return "https://github.com/$($S.Repo)/releases/download/v$($S.Version)" }
    return "https://github.com/$($S.Repo)/releases/latest/download"
}

function Get-File([string]$Url, [string]$Dest) {
    Invoke-WebRequest -Uri $Url -OutFile $Dest -UseBasicParsing
}

# Expected hash of $Name from a `sha256sum *` style file, or $null.
function Get-ExpectedHash([string]$SumsFile, [string]$Name) {
    foreach ($line in Get-Content -LiteralPath $SumsFile) {
        if ($line -match '^([0-9A-Fa-f]{64})\s+\*?(.+?)\s*$' -and $Matches[2] -eq $Name) {
            return $Matches[1].ToLowerInvariant()
        }
    }
    return $null
}

function Install-Binary($S, [string]$Tmp) {
    $arch = Get-Arch
    if ($arch -eq 'arm64') {
        Write-Warn 'there is no Windows ARM64 build; installing the x64 build, which runs under emulation'
    } elseif ($arch -ne 'x64') {
        throw "unsupported architecture: $arch (only x64 and ARM64 Windows are supported)"
    }

    $asset = 'ngiab-rs-x86_64-pc-windows-msvc.zip'
    $base = Get-ReleaseBase $S
    $zip = Join-Path $Tmp $asset
    $sums = Join-Path $Tmp 'SHA256SUMS'
    Write-Step "Downloading $asset from $base"
    Get-File "$base/$asset" $zip
    Get-File "$base/SHA256SUMS" $sums

    $expected = Get-ExpectedHash $sums $asset
    if (-not $expected) { throw "$asset is not listed in SHA256SUMS" }
    $actual = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $expected) {
        throw "checksum mismatch for $asset`n  expected $expected`n  got      $actual`nrefusing to install an unverified binary"
    }

    $x = Join-Path $Tmp 'x'
    Expand-Archive -LiteralPath $zip -DestinationPath $x -Force
    $exe = Join-Path $x 'ngiab-rs.exe'
    if (-not (Test-Path -LiteralPath $exe)) { throw "$asset does not contain ngiab-rs.exe" }

    New-Item -ItemType Directory -Force -Path $S.InstallDir | Out-Null
    $dest = Join-Path $S.InstallDir 'ngiab-rs.exe'
    if (Test-Path -LiteralPath $dest) {
        try {
            Remove-Item -LiteralPath $dest -Force
        } catch {
            # A running exe can't be deleted but can be renamed; ngiab-rs removes
            # stale *.old files at startup.
            $old = "$dest.old"
            if (Test-Path -LiteralPath $old) {
                try { Remove-Item -LiteralPath $old -Force } catch { $old = "$dest.$([guid]::NewGuid().ToString('N')).old" }
            }
            Move-Item -LiteralPath $dest -Destination $old -Force
        }
    }
    Copy-Item -LiteralPath $exe -Destination $dest -Force
    return $dest
}

function Find-Binary($S) {
    $p = Join-Path $S.InstallDir 'ngiab-rs.exe'
    if (Test-Path -LiteralPath $p) { return $p }
    $cmd = Get-Command ngiab-rs -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($cmd) { return $cmd.Source }
    return $null
}

function Test-PathContains([string]$PathValue, [string]$Dir) {
    $want = $Dir.TrimEnd('\')
    foreach ($p in "$PathValue".Split(';')) {
        if ($p.Trim().TrimEnd('\') -ieq $want) { return $true }
    }
    return $false
}

# Adds the install dir to the user Path (registry value kept as REG_EXPAND_SZ so
# existing %VAR% entries survive) and to this session's $env:Path.
function Add-ToPath($S) {
    $dir = $S.InstallDir
    if (-not (Test-PathContains $env:Path $dir)) { $env:Path = "$dir;$env:Path" }
    if ($S.NoModifyPath) {
        Write-Step "NGIAB_RS_NO_MODIFY_PATH is set: add $dir to your Path yourself."
        return
    }
    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
    try {
        $current = $key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        $expanded = [Environment]::ExpandEnvironmentVariables("$current")
        if ((Test-PathContains $current $dir) -or (Test-PathContains $expanded $dir)) { return }
        $new = if ("$current".Trim()) { "$dir;$current" } else { $dir }
        $key.SetValue('Path', $new, [Microsoft.Win32.RegistryValueKind]::ExpandString)
    } finally {
        $key.Close()
    }
    # Setting any user variable through .NET broadcasts WM_SETTINGCHANGE, so new
    # shells started from Explorer see the updated Path.
    [Environment]::SetEnvironmentVariable('NGIAB_RS_PATH_REFRESH', '1', 'User')
    [Environment]::SetEnvironmentVariable('NGIAB_RS_PATH_REFRESH', $null, 'User')
    Write-Step "Added $dir to your user Path (open a new terminal to pick it up)."
}

# Re-read Path from the registry so tools winget just installed are found.
function Update-SessionPath {
    $machine = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    $user = [Environment]::GetEnvironmentVariable('Path', 'User')
    $env:Path = "$machine;$user"
}

function Find-Exe([string]$Name, [string[]]$Fallbacks) {
    $cmd = Get-Command $Name -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($cmd) { return $cmd.Source }
    foreach ($f in $Fallbacks) {
        if ($f -and (Test-Path -LiteralPath $f)) { return $f }
    }
    return $null
}

function Find-Docker {
    Find-Exe 'docker' @("$env:ProgramFiles\Docker\Docker\resources\bin\docker.exe")
}

function Find-Podman {
    Find-Exe 'podman' @("$env:ProgramFiles\RedHat\Podman\podman.exe")
}

function Test-Wsl {
    $wsl = Find-Exe 'wsl' @("$env:SystemRoot\System32\wsl.exe")
    if (-not $wsl) { return $false }
    return (Test-Native $wsl @('--status'))
}

# winget install with echo + confirmation. Returns $true on success.
function Install-WithWinget($S, [string]$Id, [string]$Name, [string]$Url) {
    $winget = Find-Exe 'winget' @()
    if (-not $winget) {
        Write-Step "winget is not available. Install $Name from: $Url"
        return $false
    }
    $wingetArgs = @('install', '-e', '--id', $Id, '--accept-package-agreements', '--accept-source-agreements')
    Write-Step ''
    Write-Step "Install ${Name}:"
    Write-Step "    winget $($wingetArgs -join ' ')"
    if (-not (Confirm-Step $S 'Run this command?')) { return $false }
    if (-not (Invoke-Native $winget $wingetArgs)) {
        Write-Err "winget could not install $Name; install it from: $Url"
        return $false
    }
    Update-SessionPath
    return $true
}

function Invoke-Pull([string]$Exe, [string]$Image) {
    Write-Step "+ $(Split-Path -Leaf $Exe) pull $Image"
    return (Invoke-Native $Exe @('pull', $Image))
}

function Get-WslNote {
    if (Test-Wsl) { return $null }
    return 'WSL2 is not set up: run "wsl --install" in an administrator terminal and reboot'
}

function Install-Docker($S, [string]$Image) {
    Write-Step ''
    Write-Step '== docker =='
    $wslNote = Get-WslNote
    if ($wslNote) { Write-Warn $wslNote }
    $docker = Find-Docker
    if (-not $docker) {
        if (-not (Install-WithWinget $S 'Docker.DockerDesktop' 'Docker Desktop' 'https://www.docker.com/products/docker-desktop/')) {
            return @('failed', 'Docker Desktop is not installed: https://www.docker.com/products/docker-desktop/')
        }
        $note = 'launch Docker Desktop once and accept the licence; the image is pulled on first run'
        if ($wslNote) { $note = "$wslNote; then $note" }
        return @('action', $note)
    }
    if (-not (Test-Native $docker @('info'))) {
        $note = 'Docker is not running: start Docker Desktop; the image is pulled on first run'
        if ($wslNote) { $note = "$wslNote; then start Docker Desktop" }
        return @('action', $note)
    }
    if (-not (Invoke-Pull $docker $Image)) { return @('failed', "could not pull $Image") }
    return @('ok', $Image)
}

function Install-Podman($S, [string]$Image) {
    Write-Step ''
    Write-Step '== podman =='
    $wslNote = Get-WslNote
    if ($wslNote) { Write-Warn $wslNote }
    $podman = Find-Podman
    if (-not $podman) {
        if (-not (Install-WithWinget $S 'RedHat.Podman' 'Podman' 'https://podman.io/docs/installation#windows')) {
            return @('failed', 'Podman is not installed: https://podman.io/docs/installation#windows')
        }
        $podman = Find-Podman
        if (-not $podman) { return @('action', 'Podman was installed; open a new terminal and run: ngiab-rs backend install podman') }
    }
    if (-not (Test-Native $podman @('info'))) {
        if ($wslNote) { return @('failed', "$wslNote; then run: ngiab-rs backend install podman") }
        $machines = Get-NativeOutput $podman @('machine', 'list', '--format', '{{.Name}}')
        if (-not "$machines".Trim()) {
            Write-Step '+ podman machine init'
            if (-not (Invoke-Native $podman @('machine', 'init'))) { return @('failed', 'podman machine init failed') }
        }
        Write-Step '+ podman machine start'
        if (-not (Invoke-Native $podman @('machine', 'start'))) { return @('failed', 'podman machine start failed') }
        if (-not (Test-Native $podman @('info'))) { return @('failed', 'podman info still fails after starting the machine') }
    }
    if (-not (Invoke-Pull $podman $Image)) { return @('failed', "could not pull $Image") }
    return @('ok', $Image)
}

function Get-RecommendedBackend {
    $docker = Find-Docker
    if ($docker -and (Test-Native $docker @('info'))) { return 'docker' }
    $podman = Find-Podman
    if ($podman -and (Test-Native $podman @('info'))) { return 'podman' }
    return 'docker'
}

# Parse "docker,podman" / "1 2" / "all" / "none". Returns an array, or $null if invalid.
function ConvertTo-Backends([string]$Text) {
    $want = [ordered]@{ local = $false; docker = $false; podman = $false }
    foreach ($tok in ($Text.ToLowerInvariant() -split '[,\s]+')) {
        switch ($tok) {
            '' { }
            'none' { }
            '0' { }
            'local' { $want.local = $true }
            { $_ -in @('1', 'docker') } { $want.docker = $true }
            { $_ -in @('2', 'podman') } { $want.podman = $true }
            'all' { $want.docker = $true; $want.podman = $true }
            default { return $null }
        }
    }
    # The comma keeps an empty or one-element result an array.
    return , @($want.Keys | Where-Object { $want[$_] })
}

function Select-Backends($S, [string]$Recommended, [string]$Image) {
    Write-Host ''
    Write-Host 'Which backends should ngiab-rs use? (choose any number)'
    Write-Host '     local   - not available on Windows (model libraries are Linux-only)'
    Write-Host "  1) docker  - runs each tool in the $Image image."
    Write-Host '                Installs Docker Desktop with winget if missing (needs WSL2), pulls the image.'
    Write-Host '  2) podman  - like docker but daemonless.'
    Write-Host '                Installs Podman with winget if missing, creates a podman machine, pulls the image.'
    Write-Host ''
    for ($i = 0; $i -lt 3; $i++) {
        $answer = "$(Read-Host "Enter numbers or names (e.g. `"1 2`", `"all`", `"none`"; Enter = $Recommended)")".Trim()
        if (-not $answer) { $answer = $Recommended }
        $picked = ConvertTo-Backends $answer
        if ($null -ne $picked) { return , $picked }
        Write-Host "Didn't understand `"$answer`"."
    }
    Write-Host 'No valid choice; not setting up any backend.'
    return , @()
}

function Invoke-Installer {
    $S = Get-Settings
    # Windows PowerShell 5.1 defaults to TLS 1.0/1.1, which GitHub refuses.
    try {
        [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    } catch {
        Write-Warn 'could not enable TLS 1.2; downloads may fail'
    }

    $tmp = Join-Path ([IO.Path]::GetTempPath()) ("ngiab-rs-install-" + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Force -Path $tmp | Out-Null
    try {
        if ($S.SkipBinary) {
            $bin = Find-Binary $S
            if (-not $bin) { throw "NGIAB_RS_SKIP_BINARY: ngiab-rs.exe is not in $($S.InstallDir) or on Path" }
        } else {
            $bin = Install-Binary $S $tmp
            Add-ToPath $S
        }

        $versionLine = Get-NativeOutput $bin @('--version')
        if (-not $versionLine) { throw "the installed binary does not run: $bin --version" }
        $installed = ("$versionLine".Trim() -split '\s+')[1]
        if (-not $installed) { throw "could not read the version from: $versionLine" }
        $installed = $installed.TrimStart('v')
        if (-not $S.SkipBinary) { Write-Step "Installed ngiab-rs $installed to $bin" }
        $image = if ($env:NGIAB_RS_IMAGE) { $env:NGIAB_RS_IMAGE } else { "docker.io/joshcu/ngiab-rs:$installed" }

        $selected = @()
        if ($S.Backends) {
            $selected = ConvertTo-Backends $S.Backends
            if ($null -eq $selected) {
                throw "NGIAB_RS_BACKENDS: unknown backend in '$($S.Backends)' (expected docker, podman, all or none)"
            }
        } elseif ($S.Yes) {
            $selected = @(Get-RecommendedBackend)
            Write-Step "Setting up the recommended backend: $($selected -join ', ') (NGIAB_RS_YES)"
        } elseif ($S.Interactive) {
            $selected = Select-Backends $S (Get-RecommendedBackend) $image
        } else {
            $rec = Get-RecommendedBackend
            Write-Step ''
            Write-Step 'Not interactive, so no backend was set up. To set one up later, run:'
            Write-Step "    ngiab-rs backend install $rec"
        }

        $results = [ordered]@{}
        foreach ($k in $selected) {
            switch ($k) {
                'local' { $results[$k] = @('unsupported', 'not available on Windows (model libraries are Linux-only)') }
                'docker' { $results[$k] = Install-Docker $S $image }
                'podman' { $results[$k] = Install-Podman $S $image }
            }
        }

        $record = @($results.Keys | Where-Object { @('ok', 'action') -contains $results[$_][0] })
        if ($record.Count -gt 0) {
            if (-not (Invoke-Native $bin (@('backend', 'record') + $record))) {
                Write-Warn "could not record the backends in install.json (ngiab-rs backend record $($record -join ' '))"
            }
        }

        $failed = $false
        if ($results.Count -gt 0) {
            Write-Host ''
            Write-Host 'Backend summary'
            Write-Host ('  {0,-8} {1,-12} {2}' -f 'BACKEND', 'RESULT', 'NOTES')
            foreach ($k in $results.Keys) {
                $r = $results[$k]
                $label = switch ($r[0]) { 'ok' { 'ok' } 'action' { 'needs action' } 'failed' { 'FAILED' } default { $r[0] } }
                if ($r[0] -eq 'failed') { $failed = $true }
                Write-Host ('  {0,-8} {1,-12} {2}' -f $k, $label, $r[1])
            }
        }
        Write-Host ''
        Write-Host 'Get started:  ngiab-rs C:\path\to\data_folder   (ngiab-rs --help for more)'
        if ($failed) { return 1 }
        return 0
    } catch {
        Write-Err "$_"
        return 1
    } finally {
        Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }
}

Invoke-Installer
} | Select-Object -Last 1  # the exit code, even if something above leaked output

if ($MyInvocation.MyCommand.Path) {
    # Run with -File (the pinned re-run from `ngiab-rs backend install`): report the result.
    exit $ngiabRsExit
} elseif ($ngiabRsExit -ne 0) {
    Write-Host 'ngiab-rs installation did not complete; see the messages above.'
}
Remove-Variable ngiabRsExit -ErrorAction SilentlyContinue
