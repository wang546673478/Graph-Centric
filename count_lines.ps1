# Count lines in all .rs files under src/
Get-ChildItem -Recurse -Filter "*.rs" -Path "src" | ForEach-Object {
    $lines = (Get-Content $_.FullName | Measure-Object -Line).Lines
    Write-Output ("{0} {1}" -f $_.FullName, $lines)
}
