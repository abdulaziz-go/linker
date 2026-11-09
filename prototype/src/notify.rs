use std::process::Command;

pub fn notify(title: &str, body: &str){
    #[cfg(target_os="linux")]
    {
        let _ = Command::new("notify-send")
            .arg("--app-name=Linker")
            .arg(title)
            .arg(body)
            .spawn();
    }

    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "display notification \"{}\" with title \"{}\"",
            body.replace('"', "'"),
            title.replace('"', "'"),
        );

        let  _ = Command::new("osascript").arg("-e").arg(script).spawn();
    }
}