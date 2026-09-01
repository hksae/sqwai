/// Windows toast notification without extra dependencies: PowerShell + WinRT.
/// Fire-and-forget; silently does nothing off-Windows or on failure.
#[cfg(windows)]
pub fn windows_toast(title: &str, body: &str) {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    let ps_escape = |s: &str| s.replace(['\r', '\n'], " ").replace('\'', "''");
    let title = ps_escape(title);
    let body = ps_escape(body);
    let script = format!(
        r#"
[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null
[Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, ContentType = WindowsRuntime] | Out-Null
$x = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02)
$t = $x.GetElementsByTagName('text')
$t.Item(0).AppendChild($x.CreateTextNode('{title}')) | Out-Null
$t.Item(1).AppendChild($x.CreateTextNode('{body}')) | Out-Null
$n = [Windows.UI.Notifications.ToastNotification]::new($x)
[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('sqwai').Show($n)
"#
    );
    let _ = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

#[cfg(not(windows))]
pub fn windows_toast(_title: &str, _body: &str) {}
