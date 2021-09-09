use transaction_processor::TransactionProcessor;
use counter::Counter;
use blockthread::BlockThread;
use entry::Entry;
use ledger::{Block, LedgerWriter};
use log::Level;
use result::{Error, Result};
use service::Service;
use signature::Keypair;
use std::cmp;
use std::net::UdpSocket;
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, RwLock};
use std::thread::{self, Builder, JoinHandle};
use std::time::{Duration, Instant};
use streamer::responder;
use timing::{duration_as_ms, duration_as_s};
use vote_stage::send_leader_vote;

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum WriteStageReturnType {
    LeaderRotation,
    ChannelDisconnected,
}

pub struct WriteStage {
    thread_hdls: Vec<JoinHandle<()>>,
    write_thread: JoinHandle<WriteStageReturnType>,
}

impl WriteStage {
    
    fn find_leader_rotation_index(
        blockthread: &Arc<RwLock<BlockThread>>,
        leader_rotation_interval: u64,
        entry_height: u64,
        mut new_entries: Vec<Entry>,
    ) -> (Vec<Entry>, bool) {
        let new_entries_length = new_entries.len();

        
        let mut i = 0;
        let mut is_leader_rotation = false;

        loop {
            if (entry_height + i as u64) % leader_rotation_interval == 0 {
                let rblockthread = blockthread.read().unwrap();
                let my_id = rblockthread.my_data().id;
                let next_leader = rblockthread.get_scheduled_leader(entry_height + i as u64);
                if next_leader != Some(my_id) {
                    is_leader_rotation = true;
                    break;
                }
            }

            if i == new_entries_length {
                break;
            }

            
            let entries_until_leader_rotation =
                leader_rotation_interval - (entry_height % leader_rotation_interval);

            
            i += cmp::min(
                entries_until_leader_rotation as usize,
                new_entries_length - i,
            );
        }

        new_entries.truncate(i as usize);

        (new_entries, is_leader_rotation)
    }

    
    pub fn write_and_send_entries(
        blockthread: &Arc<RwLock<BlockThread>>,
        ledger_writer: &mut LedgerWriter,
        entry_sender: &Sender<Vec<Entry>>,
        entry_receiver: &Receiver<Vec<Entry>>,
        entry_height: &mut u64,
        leader_rotation_interval: u64,
    ) -> Result<()> {
        let mut ventries = Vec::new();
        let mut received_entries = entry_receiver.recv_timeout(Duration::new(1, 0))?;
        let now = Instant::now();
        let mut num_new_entries = 0;
        let mut num_txs = 0;

        loop {
            
            let (new_entries, is_leader_rotation) = Self::find_leader_rotation_index(
                blockthread,
                leader_rotation_interval,
                *entry_height + num_new_entries as u64,
                received_entries,
            );

            num_new_entries += new_entries.len();
            ventries.push(new_entries);

            if is_leader_rotation {
                break;
            }

            if let Ok(n) = entry_receiver.try_recv() {
                received_entries = n;
            } else {
                break;
            }
        }
        inc_new_counter_info!("write_stage-entries_received", num_new_entries);

        info!("write_stage entries: {}", num_new_entries);

        let mut entries_send_total = 0;
        let mut blockthread_votes_total = 0;

        let start = Instant::now();
        for entries in ventries {
            for e in &entries {
                num_txs += e.transactions.len();
            }
            let blockthread_votes_start = Instant::now();
            let votes = &entries.votes();
            blockthread.write().unwrap().insert_votes(&votes);
            blockthread_votes_total += duration_as_ms(&blockthread_votes_start.elapsed());

            ledger_writer.write_entries(entries.clone())?;
            
            *entry_height += entries.len() as u64;

            inc_new_counter_info!("write_stage-write_entries", entries.len());

            

            trace!("New entries? {}", entries.len());
            let entries_send_start = Instant::now();
            if !entries.is_empty() {
                inc_new_counter_info!("write_stage-recv_vote", votes.len());
                inc_new_counter_info!("write_stage-entries_sent", entries.len());
                trace!("broadcasting {}", entries.len());
                entry_sender.send(entries)?;
            }

            entries_send_total += duration_as_ms(&entries_send_start.elapsed());
        }
        inc_new_counter_info!(
            "write_stage-time_ms",
            duration_as_ms(&now.elapsed()) as usize
        );
        info!("done write_stage txs: {} time {} ms txs/s: {} entries_send_total: {} blockthread_votes_total: {}",
              num_txs, duration_as_ms(&start.elapsed()),
              num_txs as f32 / duration_as_s(&start.elapsed()),
              entries_send_total,
              blockthread_votes_total);

        Ok(())
    }

    
    pub fn new(
        keypair: Arc<Keypair>,
        transaction_processor: Arc<TransactionProcessor>,
        blockthread: Arc<RwLock<BlockThread>>,
        ledger_path: &str,
        entry_receiver: Receiver<Vec<Entry>>,
        entry_height: u64,
    ) -> (Self, Receiver<Vec<Entry>>) {
        let (vote_blob_sender, vote_blob_receiver) = channel();
        let send = UdpSocket::bind("0.0.0.0:0").expect("bind");
        let t_responder = responder(
            "write_stage_vote_sender",
            Arc::new(send),
            vote_blob_receiver,
        );
        let (entry_sender, entry_receiver_forward) = channel();
        let mut ledger_writer = LedgerWriter::recover(ledger_path).unwrap();

        let write_thread = Builder::new()
            .name("hypercube-writer".to_string())
            .spawn(move || {
                let mut last_vote = 0;
                let mut last_valid_validator_timestamp = 0;
                let id;
                let leader_rotation_interval;
                {
                    let rblockthread = blockthread.read().unwrap();
                    id = rblockthread.id;
                    leader_rotation_interval = rblockthread.get_leader_rotation_interval();
                }
                let mut entry_height = entry_height;
                loop {
                    if entry_height % (leader_rotation_interval as u64) == 0 {
                        let rblockthread = blockthread.read().unwrap();
                        let my_id = rblockthread.my_data().id;
                        let scheduled_leader = rblockthread.get_scheduled_leader(entry_height);
                        drop(rblockthread);
                        match scheduled_leader {
                            Some(id) if id == my_id => (),
                            
                            _ => {
                                
                                return WriteStageReturnType::LeaderRotation;
                            }
                        }
                    }

                    if let Err(e) = Self::write_and_send_entries(
                        &blockthread,
                        &mut ledger_writer,
                        &entry_sender,
                        &entry_receiver,
                        &mut entry_height,
                        leader_rotation_interval,
                    ) {
                        match e {
                            Error::RecvTimeoutError(RecvTimeoutError::Disconnected) => {
                                return WriteStageReturnType::ChannelDisconnected
                            }
                            Error::RecvTimeoutError(RecvTimeoutError::Timeout) => (),
                            _ => {
                                inc_new_counter_info!(
                                    "write_stage-write_and_send_entries-error",
                                    1
                                );
                                error!("{:?}", e);
                            }
                        }
                    };
                    if let Err(e) = send_leader_vote(
                        &id,
                        &keypair,
                        &transaction_processor,
                        &blockthread,
                        &vote_blob_sender,
                        &mut last_vote,
                        &mut last_valid_validator_timestamp,
                    ) {
                        inc_new_counter_info!("write_stage-leader_vote-error", 1);
                        error!("{:?}", e);
                    }
                }
            }).unwrap();

        let thread_hdls = vec![t_responder];
        (
            WriteStage {
                write_thread,
                thread_hdls,
            },
            entry_receiver_forward,
        )
    }
}

impl Service for WriteStage {
    type JoinReturnType = WriteStageReturnType;

    fn join(self) -> thread::Result<WriteStageReturnType> {
        for thread_hdl in self.thread_hdls {
            thread_hdl.join()?;
        }

        self.write_thread.join()
    }
}

#[cfg(test)]
mod tests {
    use transaction_processor::TransactionProcessor;
    use blockthread::{BlockThread, Node};
    use entry::Entry;
    use hash::Hash;
    use ledger::{genesis, next_entries_mut, read_ledger};
    use service::Service;
    use signature::{Keypair, KeypairUtil};
    use xpz_program_interface::pubkey::Pubkey;
    use std::fs::remove_dir_all;
    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::sync::{Arc, RwLock};
    use write_stage::{WriteStage, WriteStageReturnType};

    struct DummyWriteStage {
        my_id: Pubkey,
        write_stage: WriteStage,
        entry_sender: Sender<Vec<Entry>>,
        _write_stage_entry_receiver: Receiver<Vec<Entry>>,
        blockthread: Arc<RwLock<BlockThread>>,
        transaction_processor: Arc<TransactionProcessor>,
        leader_ledger_path: String,
        ledger_tail: Vec<Entry>,
    }

    fn process_ledger(ledger_path: &str, transaction_processor: &TransactionProcessor) -> (u64, Vec<Entry>) {
        let entries = read_ledger(ledger_path, true).expect("opening ledger");

        let entries = entries
            .map(|e| e.unwrap_or_else(|err| panic!("failed to parse entry. error: {}", err)));

        info!("processing ledger...");
        transaction_processor.process_ledger(entries).expect("process_ledger")
    }

    fn setup_dummy_write_stage(leader_rotation_interval: u64) -> DummyWriteStage {
        // Setup leader info
        let leader_keypair = Arc::new(Keypair::new());
        let my_id = leader_keypair.pubkey();
        let leader_info = Node::new_localhost_with_pubkey(leader_keypair.pubkey());

        let mut blockthread = BlockThread::new(leader_info.info).expect("BlockThread::new");
        blockthread.set_leader_rotation_interval(leader_rotation_interval);
        let blockthread = Arc::new(RwLock::new(blockthread));
        let transaction_processor = TransactionProcessor::new_default(true);
        let transaction_processor = Arc::new(transaction_processor);

        // Make a ledger
        let (_, leader_ledger_path) = genesis("test_leader_rotation_exit", 10_000);

        let (entry_height, ledger_tail) = process_ledger(&leader_ledger_path, &transaction_processor);

        // Make a dummy pipe
        let (entry_sender, entry_receiver) = channel();

        // Start up the write stage
        let (write_stage, _write_stage_entry_receiver) = WriteStage::new(
            leader_keypair,
            transaction_processor.clone(),
            blockthread.clone(),
            &leader_ledger_path,
            entry_receiver,
            entry_height,
        );

        DummyWriteStage {
            my_id,
            write_stage,
            entry_sender,
            // Need to keep this alive, otherwise the write_stage will detect ChannelClosed
            // and shut down
            _write_stage_entry_receiver,
            blockthread,
            transaction_processor,
            leader_ledger_path,
            ledger_tail,
        }
    }

    #[test]
    fn test_write_stage_leader_rotation_exit() {
        let leader_rotation_interval = 10;
        let write_stage_info = setup_dummy_write_stage(leader_rotation_interval);

        {
            let mut wblockthread = write_stage_info.blockthread.write().unwrap();
            wblockthread.set_scheduled_leader(leader_rotation_interval, write_stage_info.my_id);
        }

        let mut last_id = write_stage_info
            .ledger_tail
            .last()
            .expect("Ledger should not be empty")
            .id;
        let mut num_hashes = 0;

        let genesis_entry_height = write_stage_info.ledger_tail.len() as u64;

         
        for _ in genesis_entry_height..leader_rotation_interval {
            let new_entry = next_entries_mut(&mut last_id, &mut num_hashes, vec![]);
            write_stage_info.entry_sender.send(new_entry).unwrap();
        }

         
        let leader2_keypair = Keypair::new();
        let leader2_info = Node::new_localhost_with_pubkey(leader2_keypair.pubkey());

        {
            let mut wblockthread = write_stage_info.blockthread.write().unwrap();
            wblockthread.insert(&leader2_info.info);
            wblockthread.set_scheduled_leader(2 * leader_rotation_interval, leader2_keypair.pubkey());
        }

         
        for _ in 0..leader_rotation_interval {
            let new_entry = next_entries_mut(&mut last_id, &mut num_hashes, vec![]);
            write_stage_info.entry_sender.send(new_entry).unwrap();
        }

        assert_eq!(
            write_stage_info.write_stage.join().unwrap(),
            WriteStageReturnType::LeaderRotation
        );

        // Make sure the ledger contains exactly 2 * leader_rotation_interval entries
        let (entry_height, _) =
            process_ledger(&write_stage_info.leader_ledger_path, &write_stage_info.transaction_processor);
        remove_dir_all(write_stage_info.leader_ledger_path).unwrap();
        assert_eq!(entry_height, 2 * leader_rotation_interval);
    }

    #[test]
    fn test_leader_index_calculation() {
        // Set up a dummy node
        let leader_keypair = Arc::new(Keypair::new());
        let my_id = leader_keypair.pubkey();
        let leader_info = Node::new_localhost_with_pubkey(leader_keypair.pubkey());

        let leader_rotation_interval = 10;

        // An epoch is the period of leader_rotation_interval entries
        // time during which a leader is in power
        let num_epochs = 3;

        let mut blockthread = BlockThread::new(leader_info.info).expect("BlockThread::new");
        blockthread.set_leader_rotation_interval(leader_rotation_interval as u64);
        for i in 0..num_epochs {
            blockthread.set_scheduled_leader(i * leader_rotation_interval, my_id)
        }

        let blockthread = Arc::new(RwLock::new(blockthread));
        let entry = Entry::new(&Hash::default(), 0, vec![]);

        // A vector that is completely within a certain epoch should return that
        // entire vector
        let mut len = leader_rotation_interval as usize - 1;
        let mut input = vec![entry.clone(); len];
        let mut result = WriteStage::find_leader_rotation_index(
            &blockthread,
            leader_rotation_interval,
            (num_epochs - 1) * leader_rotation_interval,
            input.clone(),
        );

        assert_eq!(result, (input, false));

        // A vector that spans two different epochs for different leaders
        // should get truncated
        len = leader_rotation_interval as usize - 1;
        input = vec![entry.clone(); len];
        result = WriteStage::find_leader_rotation_index(
            &blockthread,
            leader_rotation_interval,
            (num_epochs * leader_rotation_interval) - 1,
            input.clone(),
        );

        input.truncate(1);
        assert_eq!(result, (input, true));

        // A vector that triggers a check for leader rotation should return
        // the entire vector and signal leader_rotation == false, if the
        // same leader is in power for the next epoch as well.
        len = 1;
        let mut input = vec![entry.clone(); len];
        result = WriteStage::find_leader_rotation_index(
            &blockthread,
            leader_rotation_interval,
            leader_rotation_interval - 1,
            input.clone(),
        );

        assert_eq!(result, (input, false));

        // A vector of new entries that spans two epochs should return the
        // entire vector, assuming that the same leader is in power for both epochs.
        len = leader_rotation_interval as usize;
        input = vec![entry.clone(); len];
        result = WriteStage::find_leader_rotation_index(
            &blockthread,
            leader_rotation_interval,
            leader_rotation_interval - 1,
            input.clone(),
        );

        assert_eq!(result, (input, false));

        // A vector of new entries that spans multiple epochs should return the
        // entire vector, assuming that the same leader is in power for both dynasties.
        len = (num_epochs - 1) as usize * leader_rotation_interval as usize;
        input = vec![entry.clone(); len];
        result = WriteStage::find_leader_rotation_index(
            &blockthread,
            leader_rotation_interval,
            leader_rotation_interval - 1,
            input.clone(),
        );

        assert_eq!(result, (input, false));

        // A vector of new entries that spans multiple leader epochs and has a length
        // exactly equal to the remainining number of entries before the next, different
        // leader should return the entire vector and signal that leader_rotation == true.
        len = (num_epochs - 1) as usize * leader_rotation_interval as usize + 1;
        input = vec![entry.clone(); len];
        result = WriteStage::find_leader_rotation_index(
            &blockthread,
            leader_rotation_interval,
            leader_rotation_interval - 1,
            input.clone(),
        );

        assert_eq!(result, (input, true));

        // Start at entry height == the height for leader rotation, should return
        // no entries.
        len = leader_rotation_interval as usize;
        input = vec![entry.clone(); len];
        result = WriteStage::find_leader_rotation_index(
            &blockthread,
            leader_rotation_interval,
            num_epochs * leader_rotation_interval,
            input.clone(),
        );

        assert_eq!(result, (vec![], true));
    }
}
