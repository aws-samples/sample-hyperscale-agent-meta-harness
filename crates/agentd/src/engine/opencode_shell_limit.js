// The run's command time limit for the engine's shell tool (the setting commands.timeout_seconds).
// A foreground call that names no timeout gets the run's limit. A timeout the model names (0 included) and a background call stay as they are.
// No text the model reads is changed. No network call, no child process, no file is written.
// One line on standard error says that the hook is in place.
export default {
  id: "hamh.shell-limit",
  setup: async (ctx) => {
    const ms = Number(ctx.options && ctx.options.timeoutMs);
    if (!Number.isInteger(ms) || ms < 1000 || ms > 1800000) {
      throw new Error("hamh.shell-limit: options.timeoutMs must be a whole number of milliseconds from 1000 to 1800000");
    }
    let said = false;
    await ctx.tool.hook("execute.before", (event) => {
      try {
        if (event.tool !== "shell") return;
        const input = event.input;
        if (input === null || typeof input !== "object" || Array.isArray(input) || input.background === true) return;
        if (input.timeout !== undefined && input.timeout !== null) return;
        event.input = { ...input, timeout: ms };
      } catch (error) {
        if (said) return;
        said = true;
        try {
          process.stderr.write(`hamh.shell-limit: execute.before failed: ${error && error.message ? error.message : String(error)}\n`);
        } catch {}
      }
    });
    process.stderr.write(`hamh.shell-limit: ready ${ms}\n`);
  },
};
