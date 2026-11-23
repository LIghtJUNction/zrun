use std::time::Duration;
use zrun::{Runtime, yield_now};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut runtime = Runtime::with_max_tasks(1024)?;
    let timer = runtime.timer();
    let task = runtime.spawn(async move {
        yield_now().await;
        timer.sleep(Duration::from_millis(10)).await?;
        Ok::<u32, zrun::TimerError>(42)
    })?;
    let result = runtime.block_on(task)??;
    println!("result = {result}");
    runtime.shutdown();
    Ok(())
}
