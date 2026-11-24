# Agent mode

```bash
prinia -a "which file defines the retry logic, and how many times does it retry?"
prinia -a --write "rename the function parse to parse_config everywhere"
prinia -a --run "run the tests and tell me what fails"
prinia -x "what is my public ip"     # -x = --agent --run
```

With `-a` (`--agent`) the model can look around in the current directory before it answers: it calls tools, `prinia` runs them and returns the results, and this repeats until the model gives a final answer. The answer goes to stdout like any other call, and one line per tool call goes to stderr so you can see what it did.

## Making it the default

```toml
agent = true   # use agent mode for every call at a terminal
run = true     # and let it run commands (`write = true` for file edits)
```

The model then decides per question whether to call a tool. The `agent` setting only applies when both stdin and stdout are terminals, so pipelines stay plain filters; `-a` forces it anywhere, `--no-agent` turns it off for one call. `--json` and `--check` never use it. `run` and `write` are the same permissions as the flags: commands still ask first unless they are read-only, but file writes do not, so enable `write` only where changes can be reviewed.

## What it can do

| Tool | Needs | What it does |
| :--- | :--- | :--- |
| `read_file` | nothing | Reads a text file (regular files up to 10 MB), with line numbers, optionally a line range |
| `list_dir` | nothing | Lists a directory |
| `search` | nothing | Finds lines containing a text (case-insensitive) under a path; skips hidden directories, `target` and `node_modules` |
| `write_file`, `replace` | `--write` | Creates or overwrites a file; replaces one exact, unique piece of text |
| `run_command` | `--run` | Runs a shell command (asking first unless read-only) in the directory and returns its output and exit code; stopped after 60 seconds |

Reading is the default and changes nothing. Writing and running commands are separate switches, so you can allow one without the other. `--run` asks before each command on your terminal (`[y/N]`), except a pipeline of read-only commands (`ls`, `cat`, `head`, `find`, `grep`, `rg`, `sort`, `wc`, `date`, ...), which runs straight away. Anything with `;`, `&&`, `$(...)`, redirects, or flags that write or run a program (`find -exec`, `-delete`, `sort -o`, `rg --pre`) asks. `git` always asks, because a repository's own config can make even `git status` run a program. With no terminal to ask on, such a command is refused and the model is told. `-y` (`--yes`) runs everything without asking. `--write` has no per-file confirmation, so use it in a git repository where changes can be reviewed.

All file tools stay inside the directory you ran the command in. A path that leaves it, by `..`, an absolute path or a symbolic link, is refused. `run_command` is not limited this way, since a shell command can do anything your account can.

The model sees the contents of every file it reads, so they are sent to the provider like any other prompt.

## Limits and recording

A session stops after 25 tool rounds (`agent_steps`). Tool output is cut at 20000 characters. Caching and `-c` context do not apply; the whole session is saved to the history as one turn in agent mode, with the final answer as its response.

The tools work with Anthropic and OpenAI-compatible providers, but the model has to support tool calling. Small local models often do it poorly.
