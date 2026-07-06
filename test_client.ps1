$pipe = New-Object System.IO.Pipes.NamedPipeClientStream(".", "som-tmux-dnk", [System.IO.Pipes.PipeDirection]::InOut)
$pipe.Connect(3000)
Write-Host "connected"

function Send-Message($json) {
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($json)
    $len = [BitConverter]::GetBytes([uint32]$bytes.Length)
    $pipe.Write($len, 0, 4)
    $pipe.Write($bytes, 0, $bytes.Length)
    $pipe.Flush()
}

function Read-Message() {
    $lenBytes = New-Object byte[] 4
    $lenOffset = 0
    while ($lenOffset -lt 4) {
        $read = $pipe.Read($lenBytes, $lenOffset, 4 - $lenOffset)
        $lenOffset += $read
    }
    $len = [BitConverter]::ToUInt32($lenBytes, 0)
    $buf = New-Object byte[] $len
    $offset = 0
    while ($offset -lt $len) {
        $read = $pipe.Read($buf, $offset, $len - $offset)
        $offset += $read
    }
    return [System.Text.Encoding]::UTF8.GetString($buf)
}

$msg = '{"NewSession":{"program":"C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe","args":["-NoLogo","-NoExit"],"cwd":"C:\\Users\\dnk","cols":80,"rows":24}}'
Write-Host "sending: $msg"
Send-Message $msg
$reply = Read-Message
Write-Host "reply: $reply"

$replyObj = $reply | ConvertFrom-Json
$sessionId = $replyObj.SessionCreated.session_id
Start-Sleep -Milliseconds 2000

$attachMsg = "{`"Attach`":{`"session_id`":`"$sessionId`",`"cols`":80,`"rows`":24}}"
Write-Host "sending: $attachMsg"
Send-Message $attachMsg
$attachReply = Read-Message
Write-Host "attach reply: $attachReply"

# Send keystrokes for "echo interactive-test-works" + Enter, encoded as the
# Write ClientMessage's bytes array (JSON array of u8).
$cmdText = "echo interactive-test-works`r"
$cmdBytes = [System.Text.Encoding]::UTF8.GetBytes($cmdText)
$bytesJson = ($cmdBytes -join ",")
$writeMsg = "{`"Write`":{`"session_id`":`"$sessionId`",`"bytes`":[$bytesJson]}}"
Start-Sleep -Milliseconds 3000
Write-Host "sending write: $writeMsg"
Send-Message $writeMsg

Write-Host "waiting for live grid updates until we see the echoed text (or 20 tries)..."
$found = $false
for ($i = 0; $i -lt 20; $i++) {
    Write-Host "  [$i] calling Read-Message..."
    $update = Read-Message
    Write-Host "  [$i] got $($update.Length) chars"
    if ($update -like "*interactive-test-works*") {
        Write-Host "FOUND expected text in update $i"
        $found = $true
        break
    }
}
if (-not $found) {
    Write-Host "did NOT find expected text after 20 updates"
}

Write-Host "sending CloseSession"
$closeMsg = "{`"CloseSession`":{`"session_id`":`"$sessionId`"}}"
Send-Message $closeMsg
$closeReply = Read-Message
Write-Host "close reply: $closeReply"

$pipe.Close()
