# Find unwrap() and unsafe{} occurrences in .rs files
param([string]$Pattern)
Get-ChildItem -Recurse -Filter "*.rs" -Path "src" | ForEach-Object {
    $content = Get-Content $_.FullName -Raw
    $unwrap_matches = [regex]::Matches($content, "\bunwrap\(\)")
    $unsafe_matches = [regex]::Matches($content, "unsafe\s*\{")
    if ($unwrap_matches.Count -gt 0 -or $unsafe_matches.Count -gt 0) {
        Write-Output ("{0} unwrap():{1} unsafe:{{{2}}" -f $_.FullName, $unwrap_matches.Count, $unsafe_matches.Count)
    }
}
