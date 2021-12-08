//! The paint state. This is the screen where you paint on the canvas with other people.

mod actions;
mod tool_bar;
mod tools;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use netcanv_protocol::relay::PeerId;
use netcanv_renderer::paws::{
   point, vector, AlignH, AlignV, Color, Layout, Rect, Renderer, Vector,
};
use netcanv_renderer::{BlendMode, Font, RenderBackend};
use nysa::global as bus;

use crate::app::paint::actions::ActionArgs;
use crate::app::paint::tool_bar::ToolbarArgs;
use crate::app::paint::tools::KeyShortcutAction;
use crate::app::*;
use crate::assets::*;
use crate::backend::Backend;
use crate::clipboard;
use crate::common::*;
use crate::net::peer::{self, Peer};
use crate::net::timer::Timer;
use crate::paint_canvas::*;
use crate::ui::view::layout::DirectionV;
use crate::ui::view::{Dimension, View};
use crate::ui::wm::WindowManager;
use crate::ui::*;
use crate::viewport::Viewport;

use self::actions::SaveToFileAction;
use self::tool_bar::{ToolId, Toolbar};
use self::tools::{BrushTool, Net, SelectionTool, ToolArgs};

/// A log message in the lower left corner.
///
/// These are used for displaying errors and joined/left messages.
type Log = Vec<(String, Instant)>;

/// A small tip in the upper left corner.
///
/// These are used for displaying the panning and zoom level.
struct Tip {
   text: String,
   created: Instant,
   visible_duration: Duration,
}

/// The state of a chunk download.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChunkDownload {
   NotDownloaded,
   Queued,
   Requested,
   Downloaded,
}

/// A bus message requesting a chunk download.
struct RequestChunkDownload((i32, i32));

/// The paint app state.
pub struct State {
   assets: Assets,

   paint_canvas: PaintCanvas,

   actions: Vec<Box<dyn actions::Action>>,
   save_to_file: Option<PathBuf>,
   last_autosave: Instant,

   peer: Peer,
   update_timer: Timer,
   chunk_downloads: HashMap<(i32, i32), ChunkDownload>,

   fatal_error: bool,
   log: Log,
   tip: Tip,

   panning: bool,
   viewport: Viewport,

   canvas_view: View,
   bottom_bar_view: View,

   overflow_menu: ContextMenu,
   toolbar: Toolbar,
   wm: WindowManager,
}

macro_rules! log {
   ($log:expr, $($arg:tt)*) => {
      $log.push((format!($($arg)*), Instant::now()))
   };
}

impl State {
   /// The network communication tick interval.
   pub const TIME_PER_UPDATE: Duration = Duration::from_millis(50);

   /// The height of the bottom bar.
   const BOTTOM_BAR_SIZE: f32 = 32.0;

   /// The amount of padding applied around the canvas area, when laying out elements on top of it.
   const CANVAS_INNER_PADDING: f32 = 8.0;

   /// Creates a new paint state.
   pub fn new(
      assets: Assets,
      peer: Peer,
      image_path: Option<PathBuf>,
      renderer: &mut Backend,
   ) -> Result<Self, (anyhow::Error, Assets)> {
      let mut wm = WindowManager::new();
      let mut this = Self {
         assets,

         paint_canvas: PaintCanvas::new(),

         actions: Vec::new(),

         peer,
         update_timer: Timer::new(Self::TIME_PER_UPDATE),
         chunk_downloads: HashMap::new(),

         save_to_file: None,
         last_autosave: Instant::now(),

         fatal_error: false,
         log: Log::new(),
         tip: Tip {
            text: "".into(),
            created: Instant::now(),
            visible_duration: Default::default(),
         },

         panning: false,
         viewport: Viewport::new(),

         canvas_view: View::new((Dimension::Percentage(1.0), Dimension::Rest(1.0))),
         bottom_bar_view: View::new((Dimension::Percentage(1.0), Self::BOTTOM_BAR_SIZE)),

         overflow_menu: ContextMenu::new((256.0, 0.0)), // Vertical is filled in later
         toolbar: Toolbar::new(&mut wm),
         wm,
      };
      this.register_tools(renderer);
      this.register_actions(renderer);

      if let Some(path) = image_path {
         if let Err(error) = this.paint_canvas.load(renderer, &path) {
            return Err((error, this.assets));
         }
      }

      if this.peer.is_host() {
         log!(this.log, "Welcome to your room!");
         log!(
            this.log,
            "To invite friends, send them the room ID shown in the bottom right corner of your screen."
         );
      }

      Ok(this)
   }

   /// Registers all the tools.
   fn register_tools(&mut self, renderer: &mut Backend) {
      let _selection = self.toolbar.add_tool(SelectionTool::new(renderer));
      let brush = self.toolbar.add_tool(BrushTool::new(renderer));

      // Set the default tool to the brush.
      self.toolbar.set_current_tool(brush);
   }

   /// Registers all the actions and calculates the layout height of the overflow menu.
   fn register_actions(&mut self, renderer: &mut Backend) {
      self.actions.push(Box::new(SaveToFileAction::new(renderer)));

      let room_id_height = 108.0;
      let separator_height = 8.0 * 2.0;
      let action_height = 32.0;
      let action_margin = 4.0;
      let actions_height = action_height * self.actions.len() as f32
         + action_margin * (self.actions.len() - 1) as f32
         + 4.0;
      self.overflow_menu.view.dimensions.vertical =
         Dimension::Constant(room_id_height + separator_height + actions_height);
   }

   /// Sets the current tool to the one with the provided ID.
   fn set_current_tool(&mut self, renderer: &mut Backend, tool: ToolId) {
      let previous_tool = self.toolbar.current_tool();
      if tool != previous_tool {
         self.toolbar.with_tool(previous_tool, |tool| {
            tool.deactivate(renderer, &mut self.paint_canvas);
         });
         catch!(self.peer.send_select_tool(self.toolbar.clone_tool_name(tool)));
         self.toolbar.set_current_tool(tool);
         self.toolbar.with_current_tool(|tool| tool.activate());
      }
   }

   /// Requests a chunk download from the host.
   fn queue_chunk_download(chunk_position: (i32, i32)) {
      bus::push(RequestChunkDownload(chunk_position));
   }

   /// Shows a tip in the upper left corner.
   fn show_tip(&mut self, text: &str, duration: Duration) {
      self.tip = Tip {
         text: text.into(),
         created: Instant::now(),
         visible_duration: duration,
      };
   }

   /// Decodes canvas data to the given chunk.
   fn canvas_data(&mut self, ui: &mut Ui, chunk_position: (i32, i32), image_data: &[u8]) {
      catch!(self.paint_canvas.decode_network_data(ui.render(), chunk_position, image_data));
   }

   /// Processes the message log.
   fn process_log(&mut self, ui: &mut Ui) {
      self.log.retain(|(_, time_created)| time_created.elapsed() < Duration::from_secs(5));
      ui.draw(|ui| {
         let mut y = ui.height() - (self.log.len() as f32 - 1.0) * 16.0 - 8.0;
         let renderer = ui.render();
         renderer.push();
         renderer.set_blend_mode(BlendMode::Invert);
         for (entry, _) in &self.log {
            renderer.text(
               Rect::new(point(8.0, y), vector(0.0, 0.0)),
               &self.assets.sans,
               &entry,
               Color::WHITE.with_alpha(240),
               (AlignH::Left, AlignV::Bottom),
            );
            y += 16.0;
         }
         renderer.pop();
      });
   }

   fn process_tool_key_shortcuts(&mut self, ui: &mut Ui, input: &mut Input) {
      // If any of the WM's windows are focused, skip keyboard shortcuts.
      if self.wm.has_focus() {
         return;
      }

      match self.toolbar.with_current_tool(|tool| {
         tool.active_key_shortcuts(
            ToolArgs {
               ui,
               input,
               wm: &mut self.wm,
               canvas_view: &self.canvas_view,
               assets: &mut self.assets,
               net: Net::new(&self.peer),
            },
            &mut self.paint_canvas,
            &self.viewport,
         )
      }) {
         KeyShortcutAction::None => (),
         KeyShortcutAction::Success => return,
         KeyShortcutAction::SwitchToThisTool => (),
      }

      let switch_tool = self
         .toolbar
         .with_each_tool(|tool_id, tool| {
            match tool.global_key_shortcuts(
               ToolArgs {
                  ui,
                  input,
                  wm: &mut self.wm,
                  canvas_view: &self.canvas_view,
                  assets: &mut self.assets,
                  net: Net::new(&self.peer),
               },
               &mut self.paint_canvas,
               &self.viewport,
            ) {
               KeyShortcutAction::None => (),
               KeyShortcutAction::Success => return ControlFlow::Break(None),
               KeyShortcutAction::SwitchToThisTool => return ControlFlow::Break(Some(tool_id)),
            }
            ControlFlow::Continue
         })
         .flatten();

      if let Some(tool) = switch_tool {
         self.set_current_tool(ui, tool);
      }

      return;
   }

   /// Processes the paint canvas.
   fn process_canvas(&mut self, ui: &mut Ui, input: &mut Input) {
      self.canvas_view.begin(ui, input, Layout::Freeform);
      let canvas_size = ui.size();

      //
      // Input
      //

      // Panning and zooming

      match input.action(MouseButton::Middle) {
         (true, ButtonState::Pressed) if ui.hover(input) => self.panning = true,
         (_, ButtonState::Released) => self.panning = false,
         _ => (),
      }

      if self.panning {
         let delta_pan = input.previous_mouse_position() - input.mouse_position();
         self.viewport.pan_around(delta_pan);
         let pan = self.viewport.pan();
         let position = format!("{}, {}", (pan.x / 256.0).floor(), (pan.y / 256.0).floor());
         self.show_tip(&position, Duration::from_millis(100));
      }
      if let (true, Some(scroll)) = input.action(MouseScroll) {
         self.viewport.zoom_in(scroll.y);
         self.show_tip(
            &format!("{:.0}%", self.viewport.zoom() * 100.0),
            Duration::from_secs(3),
         );
      }

      // Drawing & key shortcuts

      self.process_tool_key_shortcuts(ui, input);

      self.toolbar.with_current_tool(|tool| {
         tool.process_paint_canvas_input(
            ToolArgs {
               ui,
               input,
               wm: &mut self.wm,
               canvas_view: &self.canvas_view,
               assets: &self.assets,
               net: Net::new(&mut self.peer),
            },
            &mut self.paint_canvas,
            &self.viewport,
         )
      });

      //
      // Rendering
      //

      ui.draw(|ui| {
         ui.render().push();
         let Vector {
            x: width,
            y: height,
         } = ui.size();
         ui.render().translate(vector(width / 2.0, height / 2.0));
         ui.render().scale(vector(self.viewport.zoom(), self.viewport.zoom()));
         ui.render().translate(-self.viewport.pan());
         self.paint_canvas.draw_to(ui.render(), &self.viewport, canvas_size);
         ui.render().pop();

         ui.render().push();
         for (&address, mate) in self.peer.mates() {
            if let Some(tool_name) = &mate.tool {
               if let Some(tool_id) = self.toolbar.tool_by_name(tool_name) {
                  self.toolbar.with_tool(tool_id, |tool| {
                     tool.process_paint_canvas_peer(
                        ToolArgs {
                           ui,
                           input,
                           wm: &mut self.wm,
                           canvas_view: &self.canvas_view,
                           assets: &self.assets,
                           net: Net::new(&self.peer),
                        },
                        &self.viewport,
                        address,
                     );
                  });
               }
            }
         }
         ui.render().pop();

         self.toolbar.with_current_tool(|tool| {
            tool.process_paint_canvas_overlays(
               ToolArgs {
                  ui,
                  input,
                  wm: &mut self.wm,
                  canvas_view: &self.canvas_view,
                  assets: &self.assets,
                  net: Net::new(&mut self.peer),
               },
               &self.viewport,
            );
         });
      });
      if self.tip.created.elapsed() < self.tip.visible_duration {
         ui.push(ui.size(), Layout::Freeform);
         ui.pad((16.0, 16.0));
         ui.push((72.0, 32.0), Layout::Freeform);
         ui.fill(Color::BLACK.with_alpha(192));
         ui.text(
            &self.assets.sans,
            &self.tip.text,
            Color::WHITE,
            (AlignH::Center, AlignV::Middle),
         );
         ui.pop();
         ui.pop();
      }

      self.process_log(ui);

      self.canvas_view.end(ui);

      //
      // Networking
      //

      self.update_timer.tick();
      while self.update_timer.update() {
         // Tool updates
         self.toolbar.with_current_tool(|tool| {
            catch!(tool.network_send(tools::Net {
               peer: &mut self.peer
            }))
         });
         // Chunk downloading
         if self.save_to_file.is_some() {
            // FIXME: Regression introduced in 0.3.0: saving does not require all chunks to be
            // downloaded.
            // There's some internal debate I've been having on the topic od downloading all chunks
            // when the user requests a save. The main issue I see is that on large canvases
            // downloading all chunks may stall the host for too long, lagging everything to death.
            // If a client wants to download all the chunks, they should probably just explore
            // enough of the canvas such that all the chunks get loaded.
            catch!(self.paint_canvas.save(Some(&self.save_to_file.as_ref().unwrap())));
            self.last_autosave = Instant::now();
            self.save_to_file = None;
         } else {
            for chunk_position in self.viewport.visible_tiles(Chunk::SIZE, canvas_size) {
               if let Some(state) = self.chunk_downloads.get_mut(&chunk_position) {
                  if *state == ChunkDownload::NotDownloaded {
                     Self::queue_chunk_download(chunk_position);
                     *state = ChunkDownload::Queued;
                  }
               }
            }
         }
      }
   }

   /// Processes the bottom bar.
   fn process_bar(&mut self, ui: &mut Ui, input: &mut Input) {
      self.bottom_bar_view.begin(ui, input, Layout::Horizontal);

      ui.fill(self.assets.colors.panel);
      ui.pad((8.0, 0.0));

      // Tool

      self.toolbar.with_current_tool(|tool| {
         tool.process_bottom_bar(ToolArgs {
            ui,
            input,
            wm: &mut self.wm,
            canvas_view: &self.canvas_view,
            assets: &self.assets,
            net: Net::new(&mut self.peer),
         });
      });

      //
      // Right side
      // Note that elements in HorizontalRev go from right to left rather than left to right.
      //

      // TODO: move this to an overflow menu

      ui.push((ui.remaining_width(), ui.height()), Layout::HorizontalRev);

      if Button::with_icon(
         ui,
         input,
         ButtonArgs {
            height: ui.height(),
            colors: &self.assets.colors.action_button,
            corner_radius: 0.0,
         },
         &self.assets.icons.navigation.menu,
      )
      .clicked()
      {
         self.overflow_menu.toggle();
      }

      ui.pop();

      self.bottom_bar_view.end(ui);
   }

   /// Processes the overflow menu.
   fn process_overflow_menu(&mut self, ui: &mut Ui, input: &mut Input) {
      if self
         .overflow_menu
         .begin(
            ui,
            input,
            ContextMenuArgs {
               colors: &self.assets.colors.context_menu,
            },
         )
         .is_open()
      {
         ui.pad(8.0);

         // Room ID display

         ui.push((ui.width(), 0.0), Layout::Vertical);
         ui.pad((8.0, 0.0));
         ui.space(8.0);

         ui.vertical_label(
            &self.assets.sans,
            "Room ID",
            self.assets.colors.text,
            AlignH::Left,
         );
         ui.space(8.0);

         let id_text = format!("{}", self.peer.room_id().unwrap());
         ui.push((ui.width(), 32.0), Layout::HorizontalRev);
         if Button::with_icon(
            ui,
            input,
            ButtonArgs {
               height: ui.height(),
               colors: &self.assets.colors.action_button,
               corner_radius: 0.0,
            },
            &self.assets.icons.navigation.copy,
         )
         .clicked()
         {
            log!(self.log, "Room ID copied to clipboard");
            catch!(clipboard::copy_string(id_text.clone()));
         }
         ui.horizontal_label(
            &self.assets.monospace.with_size(24.0),
            &id_text,
            self.assets.colors.text,
            Some((ui.remaining_width(), AlignH::Center)),
         );
         ui.pop();

         ui.fit();
         ui.pop();
         ui.space(4.0);

         // Room host display

         ui.push((ui.width(), 32.0), Layout::Horizontal);
         ui.icon(
            if self.peer.is_host() {
               &self.assets.icons.peer.host
            } else {
               &self.assets.icons.peer.client
            },
            self.assets.colors.text,
            Some(vector(ui.height(), ui.height())),
         );
         ui.space(4.0);
         if self.peer.is_host() {
            ui.horizontal_label(
               &self.assets.sans,
               "You are the host",
               self.assets.colors.text,
               None,
            );
         } else {
            ui.push(
               (ui.remaining_width(), self.assets.sans.height() * 2.0 + 4.0),
               Layout::Vertical,
            );
            ui.align((AlignH::Right, AlignV::Middle));
            let name = truncate_text(
               &self.assets.sans_bold,
               ui.width(),
               self.peer.host_name().unwrap_or("<unknown>"),
            );
            ui.vertical_label(
               &self.assets.sans_bold,
               &name,
               self.assets.colors.text,
               AlignH::Left,
            );
            ui.space(4.0);
            ui.vertical_label(
               &self.assets.sans,
               "is your host",
               self.assets.colors.text,
               AlignH::Left,
            );
            ui.pop();
         }
         ui.pop();

         ui.space(8.0);
         ui.push((ui.width(), 0.0), Layout::Freeform);
         ui.border_top(self.assets.colors.separator, 1.0);
         ui.pop();
         ui.space(8.0);

         for action in &mut self.actions {
            if Button::process(
               ui,
               input,
               ButtonArgs {
                  height: 32.0,
                  colors: &self.assets.colors.action_button,
                  corner_radius: 2.0,
               },
               Some(ui.width()),
               |ui| {
                  ui.push(ui.size(), Layout::Horizontal);
                  ui.icon(
                     action.icon(),
                     self.assets.colors.text,
                     Some(vector(ui.height(), ui.height())),
                  );
                  ui.space(4.0);
                  ui.horizontal_label(
                     &self.assets.sans,
                     action.name(),
                     self.assets.colors.text,
                     None,
                  );
                  ui.pop();
               },
            )
            .clicked()
            {
               if let Err(error) = action.perform(ActionArgs {
                  paint_canvas: &mut self.paint_canvas,
               }) {
                  log!(self.log, "error while performing action: {}", error);
               }
            }
            ui.space(4.0);
         }

         self.overflow_menu.end(ui);
      }
   }

   fn process_peer_message(&mut self, ui: &mut Ui, message: peer::Message) -> anyhow::Result<()> {
      use peer::MessageKind;

      match message.kind {
         MessageKind::Joined(nickname, peer_id) => {
            log!(self.log, "{} joined the room", nickname);
            if self.peer.is_host() {
               let positions = self.paint_canvas.chunk_positions();
               self.peer.send_chunk_positions(peer_id, positions)?;
            }
            // Order matters here! The tool selection packet must arrive before the packets sent
            // from the tool's `network_peer_join` event.
            self
               .peer
               .send_select_tool(self.toolbar.clone_tool_name(self.toolbar.current_tool()))?;
            self
               .toolbar
               .with_current_tool(|tool| tool.network_peer_join(Net::new(&self.peer), peer_id))?;
         }
         MessageKind::Left {
            peer_id,
            nickname,
            last_tool,
         } => {
            log!(self.log, "{} has left", nickname);
            // Make sure the tool they were last using is properly deinitialized.
            if let Some(tool) = last_tool {
               if let Some(tool_id) = self.toolbar.tool_by_name(&tool) {
                  self.toolbar.with_tool(tool_id, |tool| {
                     tool.network_peer_deactivate(
                        ui,
                        Net::new(&mut self.peer),
                        &mut self.paint_canvas,
                        peer_id,
                     )
                  })?
               }
            }
         }
         MessageKind::NewHost(name) => log!(self.log, "{} is now hosting the room", name),
         MessageKind::NowHosting => log!(self.log, "You are now hosting the room"),
         MessageKind::ChunkPositions(positions) => {
            eprintln!("received {} chunk positions", positions.len());
            for chunk_position in positions {
               self.chunk_downloads.insert(chunk_position, ChunkDownload::NotDownloaded);
            }
            // Make sure we send the tool _after_ adding the requested chunks.
            // This way if something goes wrong here and the function returns Err, at least we
            // will have queued up some chunk downloads at this point.
            self
               .peer
               .send_select_tool(self.toolbar.clone_tool_name(self.toolbar.current_tool()))?;
         }
         MessageKind::Chunks(chunks) => {
            eprintln!("received {} chunks", chunks.len());
            for (chunk_position, image_data) in chunks {
               self.canvas_data(ui, chunk_position, &image_data);
               self.chunk_downloads.insert(chunk_position, ChunkDownload::Downloaded);
            }
         }
         MessageKind::GetChunks(requester, positions) => {
            self.send_chunks(requester, &positions)?;
         }
         MessageKind::Tool(sender, name, payload) => {
            if let Some(tool_id) = self.toolbar.tool_by_name(&name) {
               self.toolbar.with_tool(tool_id, |tool| {
                  tool.network_receive(
                     ui,
                     Net::new(&mut self.peer),
                     &mut self.paint_canvas,
                     sender,
                     payload.clone(),
                  )
               })?;
            }
         }
         MessageKind::SelectTool {
            peer_id: address,
            previous_tool,
            tool,
         } => {
            eprintln!("{:?} selected tool {}", address, tool);
            // Deselect the old tool.
            if let Some(tool) = previous_tool {
               if let Some(tool_id) = self.toolbar.tool_by_name(&tool) {
                  // ↑ still waiting for if_let_chains to get stabilized.
                  self.toolbar.with_tool(tool_id, |tool| {
                     tool.network_peer_deactivate(
                        ui,
                        Net::new(&mut self.peer),
                        &mut self.paint_canvas,
                        address,
                     )
                  })?;
               }
            }
            // Select the new tool.
            if let Some(tool_id) = self.toolbar.tool_by_name(&tool) {
               eprintln!(" - valid tool - {:?}", tool_id);
               self.toolbar.with_tool(tool_id, |tool| {
                  tool.network_peer_activate(Net::new(&mut self.peer), address)
               })?;
            }
         }
      }
      Ok(())
   }

   fn send_chunks(&mut self, peer_id: PeerId, positions: &[(i32, i32)]) -> anyhow::Result<()> {
      const KILOBYTE: usize = 1024;
      const MAX_BYTES_PER_PACKET: usize = 128 * KILOBYTE;

      let mut packet = Vec::new();
      let mut bytes_of_image_data = 0;
      for &chunk_position in positions {
         if bytes_of_image_data > MAX_BYTES_PER_PACKET {
            let packet = std::mem::replace(&mut packet, Vec::new());
            bytes_of_image_data = 0;
            self.peer.send_chunks(peer_id, packet)?;
         }
         if let Some(image_data) = self.paint_canvas.network_data(chunk_position) {
            packet.push((chunk_position, image_data.to_owned()));
            bytes_of_image_data += image_data.len();
         }
      }
      self.peer.send_chunks(peer_id, packet)?;

      Ok(())
   }

   fn reflow_layout(&mut self, root_view: &View) -> () {
      // The bottom bar and the canvas.
      view::layout::vertical(
         root_view,
         &mut [&mut self.bottom_bar_view, &mut self.canvas_view],
         DirectionV::BottomToTop,
      );
      let padded_canvas = view::layout::padded(&self.canvas_view, Self::CANVAS_INNER_PADDING);

      // The overflow menu.
      view::layout::align(
         &padded_canvas,
         &mut self.overflow_menu.view,
         (AlignH::Right, AlignV::Bottom),
      );
   }
}

impl AppState for State {
   fn process(
      &mut self,
      StateArgs {
         ui,
         input,
         root_view,
      }: StateArgs,
   ) {
      ui.clear(Color::WHITE);

      // Autosaving

      for action in &mut self.actions {
         match action.process(ActionArgs {
            paint_canvas: &mut self.paint_canvas,
         }) {
            Ok(()) => (),
            Err(error) => log!(self.log, "error while processing action: {}", error),
         }
      }

      // Network

      catch!(self.peer.communicate(), as Fatal);
      for message in &bus::retrieve_all::<peer::Message>() {
         if message.token == self.peer.token() {
            catch!(self.process_peer_message(ui, message.consume()));
         }
      }

      let needed_chunks: Vec<_> = bus::retrieve_all::<RequestChunkDownload>()
         .into_iter()
         .map(|message| message.consume().0)
         .collect();
      if needed_chunks.len() > 0 {
         for &chunk_position in &needed_chunks {
            self.chunk_downloads.insert(chunk_position, ChunkDownload::Requested);
         }
         catch!(self.peer.download_chunks(needed_chunks));
      }

      // Error checking

      for message in &bus::retrieve_all::<Error>() {
         let Error(error) = message.consume();
         log!(self.log, "error: {}", error);
      }
      for _ in &bus::retrieve_all::<Fatal>() {
         self.fatal_error = true;
      }

      // Layout
      self.reflow_layout(&root_view);

      // Paint canvas
      self.process_canvas(ui, input);

      // Bars
      self.toolbar.process(ToolbarArgs {
         wm: &mut self.wm,
         parent_view: &view::layout::padded(&self.canvas_view, 8.0),
      });
      // Draw windows over the toolbar, but below the bottom bar.
      self.wm.process(ui, input, &self.assets);
      self.process_bar(ui, input);
      self.process_overflow_menu(ui, input);
   }

   fn next_state(self: Box<Self>, _renderer: &mut Backend) -> Box<dyn AppState> {
      if self.fatal_error {
         Box::new(lobby::State::new(self.assets))
      } else {
         self
      }
   }
}
