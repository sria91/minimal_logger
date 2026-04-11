# myapp_logrotate.ps1 — run via Task Scheduler
$log  = "C:\Logs\myapp\myapp.log"
$dest = "C:\Logs\myapp\myapp-$(Get-Date -f yyyyMMddHHmmss).log"

Rename-Item -Path $log -NewName $dest   # FILE_SHARE_DELETE makes this possible

# Signal the logger to reopen
$evt = [System.Threading.EventWaitHandle]::OpenExisting("Global\RustLogger_LogRotate")
$evt.Set()
$evt.Close()
