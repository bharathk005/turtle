use std::io;
use std::env;
use std::process::{self, Stdio, ExitStatus};

use tokio::{
    runtime::{Runtime, Handle},
    io::AsyncWriteExt,
    process::{Command, ChildStdin},
};
use futures_util::future::{FutureExt, RemoteHandle};
use ipc_channel::ipc::IpcError;

use crate::renderer_client::ClientId;
use crate::ipc_protocol::{ClientConnection, ServerConnection, ConnectionError, ClientRequest, ServerResponse};

use super::super::main::run_main;

/// The environment variable that is set to indicate that the current process is a server process
const RENDERER_PROCESS_ENV_VAR: &str = "RUN_TURTLE_CANVAS";

/// Spawns the task/process responsible for handling and responding to client requests
///
/// Also manages the client connection used for communicating with the server
#[derive(Debug)]
pub struct RendererServer {
    /// The spawned server process
    proc: RendererServerProcess,
    /// The connection to the spawned sever process
    ///
    /// This will no longer send messages after the server process has terminated.
    conn: ClientConnection,
}

impl RendererServer {
    /// Runs any initialization logic required at the beginning of the program
    pub fn start() {
        // If this environment variable is present, this process is taken over so that no other
        // code runs after run_main(). This allows us to ship one executable that appears to
        // have two separate processes.
        //
        // This implementation detail is why we request that users run start() at the beginning of
        // their programs. When we spawn the same executable, we don't pass along any environment,
        // input or command line arguments. That means that the user *needs* to run start() first or
        // else their program won't be able to run at all. This is a tradeoff of this design decision.
        if env::var(RENDERER_PROCESS_ENV_VAR).ok().as_deref() == Some("true") {
            // The runtime for driving async code
            let runtime = Runtime::new()
                .expect("unable to spawn tokio runtime to run turtle server process");

            // Run the renderer process
            // Using `async {}` because the multiprocessed backend doesn't care when the event loop
            // quits. Quit can be detected by waiting on the spawned process.
            run_main(runtime.handle().clone(), ServerConnection::connect_stdin(), async {});
            unreachable!("bug: renderer loop did not exit after finishing");
        }
    }

    /// Spawns the backend in a new task and returns the struct that will be used to
    /// interface with it.
    pub async fn spawn() -> Result<Self, ConnectionError> {
        let mut proc = RendererServerProcess::spawn()?;
        let conn = ClientConnection::new(|name| proc.send_ipc_oneshot_name(name)).await?;

        Ok(Self {proc, conn})
    }

    /// Sends a request to the server
    pub async fn send(&self, id: ClientId, req: ClientRequest) -> Result<(), ipc_channel::Error> {
        self.conn.send(id, req).await
    }

    /// Receives a response from the server
    pub async fn recv(&self) -> Result<(ClientId, ServerResponse), IpcError> {
        self.conn.recv().await
    }
}

#[derive(Debug)]
pub struct RendererServerProcess {
    /// A handle to the runtime that the process was spawned in. This is needed because a handle
    /// to the runtime can only be created when a "runtime context". Since Drop may not always run
    /// from async code, we need this to ensure we can wait on the subprocess in `task_handle`.
    /// NOTE: This creates an implicit invariant that this struct must be dropped before the
    /// runtime that it was created in is dropped. This is not an issue in normal code and will at
    /// worst cause a panic!().
    runtime_handle: Handle,
    /// A handle to the running task. This can be waited on to find out if the process exited
    /// successfully. A remote handle will also drop the future it is associated with when it is
    /// dropped. (unlike a `JoinHandle` which will detach instead.) This is important to make sure
    /// the window closes when the thread holding this struct panics.
    task_handle: Option<RemoteHandle<io::Result<ExitStatus>>>,
    /// A handle to the stdin of the child process
    child_stdin: ChildStdin,
}

impl RendererServerProcess {
    /// Spawn a new process for the renderer
    pub fn spawn() -> io::Result<Self> {
        let current_exe = env::current_exe()?;

        // The new process is the same executable as this process but with a special environment
        // variable passed in
        let mut child = Command::new(current_exe)
            .env(RENDERER_PROCESS_ENV_VAR, "true")
            // Pipe input so we can communicate with the spawned process
            //
            // stdout/stderr will be inherited from the current process
            .stdin(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;

        let child_stdin = child.stdin.take()
            .expect("bug: renderer process was not spawned with a handle to stdin");

        // Spawn a separate task for the child process so this task can continue to make progress
        // while that runs. The remote handle will drop that future when it is dropped.
        let (child, child_handle) = child.remote_handle();
        tokio::spawn(child);
        let task_handle = Some(child_handle);

        // Keep a handle to the current runtime
        let runtime_handle = Handle::current();

        Ok(Self {runtime_handle, task_handle, child_stdin})
    }

    /// Sends the IPC one shot server name to the server process
    ///
    /// This method should only be called once
    pub async fn send_ipc_oneshot_name(&mut self, name: String) -> io::Result<()> {
        self.writeln(&name).await
    }

    /// Writes the given bytes followed by a newline b'\n' to the stdin of the process
    ///
    /// Unlike `std::io::Write::write`, this returns an error in the case all of the bytes could
    /// not be written for some reason.
    async fn writeln<S: AsRef<[u8]>>(&mut self, data: S) -> io::Result<()> {
        let data = data.as_ref();
        self.child_stdin.write_all(data).await?;
        self.child_stdin.write_all(&[b'\n']).await?;

        Ok(())
    }
}

impl Drop for RendererServerProcess {
    fn drop(&mut self) {
        use std::thread;

        // If the current thread is panicking, we don't want to wait for the window to be closed
        // since the user likely has an issue to fix in their program.
        if thread::panicking() {
            // The process is configured with `kill_on_drop` so this should close the window
            return;
        }

        // If this is just a normal ending of the main thread, we want to leave the renderer
        // running so that the user can see their drawing as long as they keep the window open

        // This unwrap is safe because no struct gets dropped twice
        let task_handle = self.task_handle.take().unwrap();

        // Wait for the child process to finish
        match self.runtime_handle.block_on(task_handle) {
            Ok(proc_status) => if !proc_status.success() {
                // Propagate error code from child process or exit with status code 1
                process::exit(proc_status.code().unwrap_or(1));
            },
            Err(err) => {
                panic!("error while running child process: {}", err);
            },
        }
    }
}