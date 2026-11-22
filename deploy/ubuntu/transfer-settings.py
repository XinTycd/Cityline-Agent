import argparse
from contextlib import closing
import json
from pathlib import Path
import sqlite3


def transfer(action: str, database: Path, settings_file: Path) -> None:
    allowed = ("agents", "brand")
    uri = database.resolve().as_uri()
    if action == "export":
        with closing(sqlite3.connect(uri + "?mode=ro", uri=True)) as db:
            settings = dict(db.execute("SELECT key, value FROM system_settings WHERE key IN (?, ?)", allowed))
        agent = json.loads(settings.get("agents", "{}"))
        if any(profile.get("api_key") for profile in [agent, *(agent.get("individual") or [])]):
            raise SystemExit("存在由工作台单独保存的 Windows 模型密钥，必须先通过服务端环境配置。")
        with settings_file.open("x", encoding="utf-8") as stream:
            json.dump(settings, stream, ensure_ascii=False)
        return

    settings = json.loads(settings_file.read_text(encoding="utf-8"))
    if not isinstance(settings, dict) or any(key not in allowed for key in settings):
        raise SystemExit("工作台设置文件格式或字段不匹配。")
    if any(not isinstance(json.loads(value), dict) for value in settings.values()):
        raise SystemExit("工作台设置必须为对象。")
    with closing(sqlite3.connect(uri + "?mode=rw", uri=True)) as db, db:
        db.executemany(
            "INSERT INTO system_settings(key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            settings.items(),
        )


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("action", choices=("export", "import"))
    parser.add_argument("database", type=Path)
    parser.add_argument("settings", type=Path)
    args = parser.parse_args()
    transfer(args.action, args.database, args.settings)
