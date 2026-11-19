<#
.SYNOPSIS
    Helper script to build TrackerTop, index a repository, and open the HTML dashboard on Windows.
#>

function Invoke-TrackerTopIndex {
    param(
        [string]$RepoPath = ".",
        [switch]$Force
    )
    $argsList = @("--repo", $RepoPath, "index")
    if ($Force) { $argsList += "--force" }
    & cargo run --release -- @argsList
}

function Invoke-TrackerTopReport {
    param(
        [string]$RepoPath = "."
    )
    Invoke-TrackerTopIndex -RepoPath $RepoPath
    & cargo run --release -- --repo $RepoPath report
    & cargo run --release -- --repo $RepoPath stats
}
