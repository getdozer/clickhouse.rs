use bytes::Bytes;
use futures::TryStreamExt;
use serde::Deserialize;

use crate::{
    buflist::BufList,
    error::{Error, Result},
    response::Response,
    rowbinary,
};

const INITIAL_BUFFER_SIZE: usize = 1024;

struct CellCursor {
    response: Response,
    pending: BufList<Bytes>,
}

impl CellCursor {
    fn new(response: Response) -> Self {
        Self {
            response,
            pending: BufList::default(),
        }
    }

    #[inline(always)]
    async fn next<T>(
        &mut self,
        mut f: impl FnMut(&mut BufList<Bytes>) -> ControlFlow<T>,
    ) -> Result<Option<T>> {
        let chunks = if let Some(chunks) = self.response.chunks() {
            chunks
        } else {
            self.response.chunks_slow().await?
        };

        loop {
            match f(&mut self.pending) {
                ControlFlow::Yield(value) => {
                    self.pending.commit();
                    return Ok(Some(value));
                }
                #[cfg(feature = "watch")]
                ControlFlow::Skip => {
                    self.pending.commit();
                    continue;
                }
                ControlFlow::Retry => {
                    self.pending.rollback();
                    continue;
                }
                ControlFlow::Err(Error::NotEnoughData) => {
                    self.pending.rollback();
                }
                ControlFlow::Err(err) => return Err(err),
            }

            match chunks.try_next().await? {
                Some(chunk) => self.pending.push(chunk),
                None if self.pending.bufs_cnt() > 0 => return Err(Error::NotEnoughData),
                None => return Ok(None),
            }
        }
    }
}

enum ControlFlow<T> {
    Yield(T),
    #[cfg(feature = "watch")]
    Skip,
    Retry,
    Err(Error),
}

// XXX: it was a workaround for https://github.com/rust-lang/rust/issues/51132,
//      but introduced #24 and must be fixed.
fn workaround_51132<'a, T>(ptr: &mut T) -> &'a mut T {
    unsafe { &mut *(ptr as *mut T) }
}

// === RowBinaryCursor ===

pub(crate) struct CellBinaryCursor {
    raw: CellCursor,
    buffer: Vec<u8>,
}

impl CellBinaryCursor {
    pub(crate) fn new(response: Response) -> Self {
        Self {
            raw: CellCursor::new(response),
            buffer: vec![0; INITIAL_BUFFER_SIZE],
        }
    }

    pub(crate) async fn next<'a, 'b: 'a, T>(&'a mut self) -> Result<Option<T>>
    where
        T: Deserialize<'b>,
    {
        let buffer = &mut self.buffer;

        self.raw
            .next(|pending| {
                match rowbinary::deserialize_from(pending, &mut workaround_51132(buffer)[..]) {
                    Ok(value) => ControlFlow::Yield(value),
                    Err(Error::TooSmallBuffer(need)) => {
                        let new_len = (buffer.len() + need)
                            .checked_next_power_of_two()
                            .expect("oom");
                        buffer.resize(new_len, 0);
                        ControlFlow::Retry
                    }
                    Err(err) => ControlFlow::Err(err),
                }
            })
            .await
    }
}
