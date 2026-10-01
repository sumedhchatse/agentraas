"""`agentraas <command>`: entry point for the no-server tools."""
import sys

USAGE = """usage: agentraas <command> [options]

commands:
  wrap   exactly-once tool calls + audit log for any MCP server
         agentraas wrap -- npx -y @modelcontextprotocol/server-github
  chaos  find API calls your agent would execute twice
         agentraas chaos --mock -- python my_agent.py
  tunnel forward webhooks from a public URL to a local port
         agentraas tunnel --port 3000 --org org_acme --agent agent_1 --key ar_live_...
  apply  make an org's rules match a YAML file kept in Git
         agentraas apply -f agentraas.yaml [--dry-run] [--prune]
"""


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    if not argv or argv[0] in ("-h", "--help"):
        print(USAGE)
        return 0
    cmd, rest = argv[0], argv[1:]
    if cmd == "wrap":
        from .mcp_wrap import main as run
    elif cmd == "chaos":
        from .chaos import main as run
    elif cmd == "tunnel":
        from .tunnel import main as run
    elif cmd == "apply":
        from .apply import main as run
    else:
        print(USAGE, file=sys.stderr)
        return 2
    return run(rest)


if __name__ == "__main__":
    sys.exit(main())
