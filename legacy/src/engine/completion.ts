import type { CommandCatalog } from "./types.ts";

export type Shell = "bash" | "fish" | "zsh";

const tokens = (catalog: CommandCatalog): string[] =>
  [...new Set(catalog.commands.flatMap((command) => command.path))].toSorted();

export const generateCompletion = (
  cliName: string,
  shell: Shell,
  catalog: CommandCatalog
): string => {
  const words = tokens(catalog).join(" ");
  if (shell === "bash") {
    return `_${cliName}_completion() {
  local current="\${COMP_WORDS[COMP_CWORD]}"
  COMPREPLY=( $(compgen -W "${words}" -- "$current") )
}
complete -F _${cliName}_completion ${cliName}
`;
  }
  if (shell === "zsh") {
    return `#compdef ${cliName}
_${cliName}() {
  local -a commands
  commands=(${words})
  _describe '${cliName} commands' commands
}
compdef _${cliName} ${cliName}
`;
  }
  return `complete -c ${cliName} -f
${tokens(catalog)
  .map((token) => `complete -c ${cliName} -a '${token}'`)
  .join("\n")}
`;
};
