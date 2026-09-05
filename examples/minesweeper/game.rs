//! Hardware-independent rules. Test with `rustc --test examples/minesweeper/game.rs`.

pub const SIDE: usize = 9;
pub const CELLS: usize = SIDE * SIDE;
pub const MINES: usize = 10;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Ready,
    Playing,
    Won,
    Lost,
}

pub struct Game {
    mines: [bool; CELLS],
    revealed: [bool; CELLS],
    flagged: [bool; CELLS],
    pub state: State,
}

impl Game {
    pub fn new() -> Self {
        Self {
            mines: [false; CELLS],
            revealed: [false; CELLS],
            flagged: [false; CELLS],
            state: State::Ready,
        }
    }

    pub fn flags(&self) -> usize {
        self.flagged.iter().filter(|&&flag| flag).count()
    }

    pub fn toggle_flag(&mut self, cell: usize) -> bool {
        if cell >= CELLS || self.finished() || self.revealed[cell] {
            return false;
        }
        self.flagged[cell] = !self.flagged[cell];
        true
    }

    fn finished(&self) -> bool {
        matches!(self.state, State::Won | State::Lost)
    }

    fn neighbors(cell: usize) -> impl Iterator<Item = usize> {
        let row = cell / SIDE;
        let col = cell % SIDE;
        (row.saturating_sub(1)..=(row + 1).min(SIDE - 1)).flat_map(move |r| {
            (col.saturating_sub(1)..=(col + 1).min(SIDE - 1))
                .map(move |c| r * SIDE + c)
                .filter(move |&other| other != cell)
        })
    }

    fn adjacent(&self, cell: usize) -> u8 {
        Self::neighbors(cell).filter(|&i| self.mines[i]).count() as u8
    }

    fn place_mines(&mut self, safe: usize, seed: u64) {
        // Partial Fisher-Yates shuffle: exactly ten distinct mines, excluding
        // the first revealed cell. Tap timing supplies a fresh seed each game.
        let mut candidates = [0; CELLS - 1];
        let mut next = 0;
        for cell in 0..CELLS {
            if cell != safe {
                candidates[next] = cell;
                next += 1;
            }
        }
        let mut random = seed ^ 0x9e3779b97f4a7c15;
        if random == 0 {
            random = 1;
        }
        for i in 0..MINES {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            let j = i + (random % (candidates.len() - i) as u64) as usize;
            candidates.swap(i, j);
            self.mines[candidates[i]] = true;
        }
        self.state = State::Playing;
    }

    pub fn reveal(&mut self, cell: usize, seed: u64) -> bool {
        if cell >= CELLS || self.finished() || self.flagged[cell] || self.revealed[cell] {
            return false;
        }
        if self.state == State::Ready {
            self.place_mines(cell, seed);
        }
        self.revealed[cell] = true;
        if self.mines[cell] {
            self.state = State::Lost;
            return true;
        }

        // Bounded flood fill; mark before enqueueing so each cell is queued once.
        let mut queue = [0; CELLS];
        queue[0] = cell;
        let mut head = 0;
        let mut tail = 1;
        while head < tail {
            let current = queue[head];
            head += 1;
            if self.adjacent(current) != 0 {
                continue;
            }
            for other in Self::neighbors(current) {
                if !self.revealed[other] && !self.flagged[other] && !self.mines[other] {
                    self.revealed[other] = true;
                    queue[tail] = other;
                    tail += 1;
                }
            }
        }
        if self.revealed.iter().filter(|&&open| open).count() == CELLS - MINES {
            self.state = State::Won;
        }
        true
    }

    pub fn symbol(&self, cell: usize) -> u8 {
        if self.finished() && self.mines[cell] {
            b'*'
        } else if self.flagged[cell] {
            b'F'
        } else if !self.revealed[cell] {
            b'#'
        } else {
            match self.adjacent(cell) {
                0 => b' ',
                count => b'0' + count,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_first_cell_is_safe_and_exactly_ten_mines_are_placed() {
        for seed in [0, 1, 42, u64::MAX, 0x9e3779b97f4a7c15] {
            for first in 0..CELLS {
                let mut game = Game::new();
                assert!(game.reveal(first, seed));
                assert_eq!(game.state, State::Playing);
                assert!(!game.mines[first]);
                assert_eq!(game.mines.iter().filter(|&&mine| mine).count(), MINES);
                assert!(game.symbol(first) != b'#');
            }
        }
    }

    #[test]
    fn flags_protect_cells_and_can_be_removed() {
        let mut game = Game::new();
        assert!(game.toggle_flag(40));
        assert_eq!(game.flags(), 1);
        assert_eq!(game.symbol(40), b'F');
        assert!(!game.reveal(40, 1));
        assert_eq!(game.state, State::Ready);
        assert!(game.toggle_flag(40));
        assert_eq!(game.flags(), 0);
        assert!(game.reveal(40, 1));
        assert!(!game.toggle_flag(40));
    }

    #[test]
    fn flood_fill_reveals_empty_regions_and_numbered_boundaries_but_not_flags() {
        let mut game = Game::new();
        // Ten mines across the top, with a large empty region below.
        game.mines[..MINES].fill(true);
        game.state = State::Playing;
        game.toggle_flag(80);
        game.reveal(72, 0);
        assert_eq!(game.symbol(72), b' ');
        assert_eq!(game.symbol(18), b'1');
        assert_eq!(game.symbol(80), b'F');
        assert_eq!(game.state, State::Playing);
        assert!(!game.revealed[..MINES].iter().any(|&open| open));
        game.toggle_flag(80);
        game.reveal(80, 0);
        assert_eq!(game.state, State::Won);
        assert!(!game.reveal(0, 0));
        assert!(!game.toggle_flag(0));
    }

    #[test]
    fn hitting_a_mine_ends_game_and_new_game_resets_everything() {
        let mut game = Game::new();
        game.reveal(40, 42);
        let mine = game.mines.iter().position(|&mine| mine).unwrap();
        game.reveal(mine, 0);
        assert_eq!(game.state, State::Lost);
        assert_eq!(
            (0..CELLS).filter(|&i| game.symbol(i) == b'*').count(),
            MINES
        );
        assert!(!game.toggle_flag(0));
        assert!(!game.reveal(0, 0));
        game = Game::new();
        assert_eq!(game.state, State::Ready);
        assert_eq!(game.flags(), 0);
        assert!((0..CELLS).all(|i| game.symbol(i) == b'#'));
    }

    #[test]
    fn neighbors_do_not_wrap_at_edges() {
        assert_eq!(Game::neighbors(0).collect::<Vec<_>>(), [1, 9, 10]);
        assert_eq!(Game::neighbors(8).collect::<Vec<_>>(), [7, 16, 17]);
        assert_eq!(Game::neighbors(80).collect::<Vec<_>>(), [70, 71, 79]);
        assert_eq!(Game::neighbors(40).count(), 8);
    }
}
