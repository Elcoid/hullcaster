use std::path::PathBuf;
use std::sync::mpsc;

use crate::types::*;
use crate::config::Config;
use crate::ui::UI;
use crate::db::Database;
use crate::feeds;
use crate::downloads::{self, EpData};
use crate::play_file;
use crate::MESSAGE_TIME;

/// Enum used for communicating with other threads.
#[derive(Debug)]
pub enum MainMessage {
    UiUpdateMenus,
    UiSpawnMsgWin(String, u64),
    UiTearDown,
}

/// Main application controller, holding all of the main application
/// state and mechanisms for communicatingg with the rest of the app.
pub struct MainController {
    pub config: Config,
    pub db: Database,
    pub download_manager: downloads::DownloadManager,
    pub podcasts: LockVec<Podcast>,
    pub ui_thread: std::thread::JoinHandle<()>,
    pub tx_to_ui: mpsc::Sender<MainMessage>,
    pub tx_to_main: mpsc::Sender<Message>,
    pub rx_to_main: mpsc::Receiver<Message>,
}

impl MainController {
    /// Instantiates the main controller (used during app startup), which
    /// sets up the connection to the database, download manager, and UI
    /// thread, and reads the list of podcasts from the database.
    pub fn new(config: Config) -> MainController {
        // create transmitters and receivers for passing messages between threads
        let (tx_to_ui, rx_from_main) = mpsc::channel();
        let (tx_to_main, rx_to_main) = mpsc::channel();
        
        // get connection to the database
        let db_inst = Database::connect(&config.config_path);
    
        // set up download manager
        let tx_dl_to_main = tx_to_main.clone();
        let download_manager = downloads::DownloadManager::new(
            config.simultaneous_downloads,
            tx_dl_to_main);
    
        // create vector of podcasts, where references are checked at
        // runtime; this is necessary because we want main.rs to hold the
        // "ground truth" list of podcasts, and it must be mutable, but
        // UI needs to check this list and update the screen when
        // necessary
        let podcast_list = LockVec::new(db_inst.get_podcasts());
    
        // set up UI in new thread
        let tx_ui_to_main = mpsc::Sender::clone(&tx_to_main);
        let ui_thread = UI::spawn(config.clone(), podcast_list.clone(), rx_from_main, tx_ui_to_main);
            // TODO: Can we do this without cloning the config?

        return MainController {
            config: config,
            db: db_inst,
            download_manager: download_manager,
            podcasts: podcast_list,
            ui_thread: ui_thread,
            tx_to_ui: tx_to_ui,
            tx_to_main: tx_to_main,
            rx_to_main: rx_to_main,
        };
    }

    /// Sends the specified message to the UI, which will display at
    /// the bottom of the screen.
    pub fn msg_to_ui(&self, message: String) {
        self.tx_to_ui.send(MainMessage::UiSpawnMsgWin(
            message, MESSAGE_TIME)).unwrap();
    }

    /// Synchronize RSS feed data for one or more podcasts.
    pub fn sync(&self, pod_index: Option<usize>) {
        // We pull out the data we need here first, so we can
        // stop borrowing the podcast list as quickly as possible.
        // Slightly less efficient (two loops instead of
        // one), but then it won't block other tasks that
        // need to access the list.
        let mut pod_data = Vec::new();
        {
            let borrowed_pod_list = self.podcasts
                .borrow();
            match pod_index {
                Some(idx) => {
                    // just grab one podcast
                    let podcast = &borrowed_pod_list[idx];
                    pod_data.push((podcast.url.clone(), podcast.id));
                },
                None => {
                    // get all of 'em!
                    for podcast in borrowed_pod_list.iter() {
                        pod_data.push((podcast.url.clone(), podcast.id));
                    }
                },
            }
        }
        for data in pod_data.into_iter() {
            let url = data.0;
            let id = data.1;

            let tx_feeds_to_main = mpsc::Sender::clone(&self.tx_to_main);
            let _ = feeds::spawn_feed_checker(tx_feeds_to_main, url, id);
        }
    }

    /// Handles the application logic for adding a new podcast, or
    /// synchronizing data from the RSS feed of an existing podcast.
    pub fn add_or_sync_data(&self, pod: Podcast, update: bool) {
        let title = pod.title.clone();
        let db_result;
        let failure;

        if update {
            db_result = self.db.update_podcast(pod);
            failure = format!("Error synchronizing {}.", title);
        } else {
            db_result = self.db.insert_podcast(pod);
            failure = "Error adding podcast to database.".to_string();
        }
        match db_result {
            Ok(num_ep) => {
                *self.podcasts.borrow() = self.db.get_podcasts();
                self.tx_to_ui.send(MainMessage::UiUpdateMenus).unwrap();

                if update {
                    self.msg_to_ui(format!("Synchronized {}.", title));
                } else {
                    self.msg_to_ui(format!("Successfully added {} episodes.", num_ep));
                }
            },
            Err(_err) => self.msg_to_ui(failure),
        }
    }

    /// Attempts to execute the play command on the given podcast
    /// episode.
    pub fn play_file(&self, pod_index: usize, ep_index: usize) {
        let episode = self.podcasts.clone_episode(
            pod_index, ep_index).unwrap();

        match episode.path {
            // if there is a local file, try to play that
            Some(path) => {
                match path.to_str() {
                    Some(p) => {
                        if play_file::execute(&self.config.play_command, &p).is_err() {
                            self.msg_to_ui(
                                "Error: Could not play file. Check configuration.".to_string());
                        }
                    },
                    None => self.msg_to_ui(
                        "Error: Filepath is not valid Unicode.".to_string()),
                }
            },
            // otherwise, try to stream the URL
            None => {
                if play_file::execute(&self.config.play_command, &episode.url).is_err() {
                    self.msg_to_ui(
                        "Error: Could not stream URL.".to_string());
                }
            }
        }
    }

    /// Given a podcast and episode, it marks the given episode as
    /// played/unplayed, sending this info to the database and updating
    /// in main_ctrl.podcasts
    pub fn mark_played(&self, pod_index: usize, ep_index: usize, played: bool) {
        let mut podcast = self.podcasts.clone_podcast(pod_index).unwrap();
        let mut any_unplayed = false;

        // TODO: Try to find a way to do this without having
        // to clone the episode...
        let mut episode = podcast.episodes.clone_episode(ep_index).unwrap();
        episode.played = played;
        
        self.db.set_played_status(episode.id.unwrap(), played);
        podcast.episodes.replace(ep_index, episode).unwrap();

        {
            // recheck if there are any unplayed episodes for the
            // selected podcast
            let borrowed_ep_list = podcast
                .episodes.borrow();
            for ep in borrowed_ep_list.iter() {
                if !ep.played {
                    any_unplayed = true;
                    break;
                }
            }
        }
        if any_unplayed != podcast.any_unplayed {
            podcast.any_unplayed = any_unplayed;
            self.podcasts.replace(pod_index, podcast).unwrap();
        }
    }

    /// Given a podcast, it marks all episodes for that podcast as
    /// played/unplayed, sending this info to the database and updating
    /// in main_ctrl.podcasts
    pub fn mark_all_played(&self, pod_index: usize, played: bool) {
        let mut podcast = self.podcasts.clone_podcast(pod_index).unwrap();
        {
            let mut borrowed_ep_list = podcast
                .episodes.borrow();

            for ep in borrowed_ep_list.iter() {
                self.db.set_played_status(ep.id.unwrap(), played);
            }

            *borrowed_ep_list = self.db.get_episodes(podcast.id.unwrap());
        }

        podcast.any_unplayed = !played;
        self.podcasts.replace(pod_index, podcast).unwrap();
        self.tx_to_ui.send(MainMessage::UiUpdateMenus).unwrap();
    }

    // TODO: Right now this can't be used because the main loop is
    // borrowing the MainController object, and the last line of this
    // function mutates MainController, so the borrow checker complains.
    // pub fn download(&mut self, pod_index: usize, ep_index: Option<usize>) {
    //     // TODO: Try to do this without cloning the podcast...
    //     let podcast = self.podcasts
    //         .clone_podcast(pod_index).unwrap();
    //     let pod_title = podcast.title.clone();
    //     let borrowed_ep_list = podcast
    //         .episodes.borrow();

    //     let mut episodes = Vec::new();

    //     // if we are selecting one specific episode, just grab that one;
    //     // otherwise, loop through them all
    //     match ep_index {
    //         Some(ep_idx) => episodes.push(&borrowed_ep_list[ep_idx]),
    //         None => {
    //             for e in borrowed_ep_list.iter() {
    //                 episodes.push(e);
    //             }
    //         }
    //     }

    //     // add directory for podcast, create if it does not exist
    //     match main_ctrl.create_podcast_dir(pod_title.clone()) {
    //         Ok(path) => main_ctrl.download_manager.download_list(
    //             &episodes, &path),
    //         Err(_) => main_ctrl.msg_to_ui(
    //             format!("Could not create dir: {}", pod_title)),
    //     })
    // }

    /// Handles logic for what to do when a download successfully completes.
    pub fn download_complete(&self, ep_data: EpData) {
        let _ = self.db.insert_file(ep_data.id, &ep_data.file_path);
        {
            let pod_index = self.podcasts
                .id_to_index(ep_data.pod_id).unwrap();
            // TODO: Try to do this without cloning the podcast...
            let podcast = self.podcasts
                .clone_podcast(pod_index).unwrap();

            let ep_index = podcast.episodes
                .id_to_index(ep_data.id).unwrap();
            let mut episode = podcast.episodes.clone_episode(ep_index).unwrap();
            episode.path = Some(ep_data.file_path.clone());
            podcast.episodes.replace(ep_index, episode).unwrap();
        }

        self.tx_to_ui.send(MainMessage::UiUpdateMenus).unwrap();
    }

    /// Given a podcast title, creates a download directory for that
    /// podcast if it does not already exist.
    pub fn create_podcast_dir(&self, pod_title: String) -> Result<PathBuf, std::io::Error> {
        let mut download_path = self.config.download_path.clone();
        download_path.push(pod_title);
        return match std::fs::create_dir_all(&download_path) {
            Ok(_) => Ok(download_path),
            Err(err) => Err(err),
        }
    }
}