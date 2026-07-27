//! `mdtask-core` parses a markdown task file into a typed job tree and runs jobs
//! from it. It is embeddable, execution-capable, and dependency-free.
//!
//! A task file is ordinary markdown (a `tasks.md`, a `maskfile.md`, or a project
//! `README.md`): a heading is a job, the first fenced code block under it is the
//! script, and `Key: value` lines in the body carry metadata. The format is its
//! own grammar, a graceful superset that borrows xc's metadata vocabulary and
//! mask's runtime shape (per-fence interpreter, positional args). It reads cleanly
//! in those tools where the features overlap, but claims no compatibility.
//!
//! ```
//! let tf = mdtask_core::parse("\
//! ## greet\n\
//! \n\
//! Args: name\n\
//! \n\
//! ```sh\n\
//! echo \"hello {{ name }}\"\n\
//! ```\n");
//! let job = tf.job("greet").unwrap();
//! assert_eq!(job.args[0].name, "name");
//! ```
//!
//! Parsing is pure. A consumer sees only jobs and their metadata: interpreter
//! selection, argv building, working-directory resolution, and spawning are all
//! internal. Three entry points run a job and its `Requires:` chain: [`run`]
//! inherits stdio (streaming, for a CLI), [`run_captured`] captures the aggregated
//! output (for an embedder), and [`run_agent`] adds the agent allow gate and the
//! injection guard (for an MCP or agent surface). The parser is line-based (no
//! CommonMark dependency), so a `#` or `Key:` inside a fenced block is never
//! mistaken for structure.

mod cancel;
mod deps;
mod discover;
mod model;
mod parse;
mod run;

pub use cancel::Cancel;
pub use discover::find_task_files;
pub use model::{Arg, DepError, Job, MissingArg, Requirement, RunError, TaskFile};
pub use parse::parse;
pub use run::{agent_jobs, run, run_agent, run_agent_cancellable, run_captured};
