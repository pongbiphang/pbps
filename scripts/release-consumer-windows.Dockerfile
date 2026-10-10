# escape=`
FROM mcr.microsoft.com/windows/servercore@sha256:e18a49cbc074dfaa8e106296d51cebd62bbf6effb999f134a5c48eed1c2334e1
# Only Git is copied from the producer's tools, never its PATH or product tree.
COPY git C:/git
ENV PATH="C:\git\cmd;C:\Windows\System32;C:\Windows;C:\Windows\System32\WindowsPowerShell\v1.0"
ENV GIT_CONFIG_NOSYSTEM=1
RUN powershell.exe -NoProfile -Command "$ErrorActionPreference='Stop'; foreach ($name in @('cargo','rustc','psql','sqlcmd')) { if (Get-Command $name -ErrorAction SilentlyContinue) { throw ('Unexpected tool: '+$name) } }; git --version; if ($LASTEXITCODE) { exit $LASTEXITCODE }"
