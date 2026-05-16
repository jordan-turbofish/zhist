mod db;
mod tui;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "zhist")]
#[command(about = "Query zsh history database")]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Output in JSON format
    #[arg(short, long)]
    json: bool,

    /// Show host in text output
    #[arg(short = 'H', long)]
    host: bool,

    /// Show directory in text output
    #[arg(short, long)]
    dir: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Start interactive TUI browser
    Interactive {
        /// Initial query string
        #[arg()]
        query: Option<String>,
    },
    /// Output zsh initialization script
    Zsh,
}

fn zsh() {
    print!(
        r##"
_zhist-widget() {{
  origquery=${{BUFFER}}
  output=$( \
    HISTDB_HOST=${{HISTDB_HOST:-"'$(sql_escape ${{HOST}})'"}} \
    HISTDB_SESSION=$HISTDB_SESSION \
    HISTDB_FILE=$HISTDB_FILE \
    ZHIST_WIDGET=1 \
    zhist interactive -- "$origquery"\
  )

  if [ $? -eq 0 ]; then
    BUFFER=$output
  else
    BUFFER=$origquery
  fi

  CURSOR=$#BUFFER
  zle redisplay
}}

zle -N zhist-widget _zhist-widget
bindkey -M emacs '^r' zhist-widget
bindkey -M viins '^r' zhist-widget
"##
    );
}

fn main() {
    let args = Args::parse();

    if let Some(Command::Interactive { query }) = args.command {
        let mut app = tui::App::new(query);
        if let Some(cmd) = app.run() {
            println!("{cmd}");
        }
        return;
    }

    if let Some(Command::Zsh) = args.command {
        zsh();
        return;
    }

    let histdb_info = db::HistdbInfo::from_env();
    let conn = db::open_db(&histdb_info).expect("failed to open database");
    let entries = db::HistoryEntry::query_all(&conn).expect("failed to query history");

    if args.json {
        println!("{}", serde_json::to_string_pretty(&entries).unwrap());
    } else {
        for entry in &entries {
            let prefix = format!(
                "{:>6}  {:>10}  {:>8}",
                entry.id,
                entry.start_time,
                entry.exit_status.map_or("-".into(), |s| s.to_string()),
            );
            let mut parts = vec![prefix];
            if args.host {
                parts.push(entry.host.clone());
            }
            if args.dir {
                parts.push(entry.dir.clone());
            }
            parts.push(entry.argv.clone());
            println!("{}", parts.join("  "));
        }
    }
}
