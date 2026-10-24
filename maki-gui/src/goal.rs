//! `/goal`: keep the agent on one objective across runs. Each run that ends
//! without the agent declaring the goal met or blocked gets a nudge to carry
//! on, up to a cap, so a stuck goal cannot spend without end.

pub const MAX_RUNS: u32 = 25;
const ACHIEVED: &str = "GOAL ACHIEVED";
const BLOCKED: &str = "GOAL BLOCKED";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Goal {
    pub objective: String,
    pub runs: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Next {
    Continue(String),
    Achieved,
    Blocked,
    OutOfRuns,
}

impl Goal {
    pub fn new(objective: impl Into<String>) -> Self {
        Self {
            objective: objective.into(),
            runs: 1,
        }
    }

    pub fn kickoff(&self) -> String {
        format!(
            "Goal: {}\n\nWork on this on your own until it is fully achieved, checking your work as you go. \
             Do not stop to ask unless only the user can unblock you. When the goal is met and verified, \
             end your final message with a line that says only `{ACHIEVED}`. If you are blocked on \
             something only the user can do, say what it is and end with a line that says only `{BLOCKED}`.",
            self.objective
        )
    }

    /// Decides what follows a run that ended normally, given its last reply.
    pub fn after_run(&mut self, reply: &str) -> Next {
        let last_line = reply.trim_end().lines().last().unwrap_or_default().trim();
        let last_line = last_line.trim_matches(|c: char| c == '`' || c == '*');
        if last_line == ACHIEVED {
            return Next::Achieved;
        }
        if last_line == BLOCKED {
            return Next::Blocked;
        }
        if self.runs >= MAX_RUNS {
            return Next::OutOfRuns;
        }
        self.runs += 1;
        Next::Continue(format!(
            "Keep going toward the goal: {}\n\nPick up where you left off. When it is met and verified, \
             end with a line that says only `{ACHIEVED}`; if only the user can unblock you, end with \
             `{BLOCKED}`.",
            self.objective
        ))
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const OBJECTIVE: &str = "make the tests pass";

    #[test_case("All green.\nGOAL ACHIEVED", Next::Achieved ; "achieved")]
    #[test_case("Done.\n\n`GOAL ACHIEVED`\n", Next::Achieved ; "achieved_in_code")]
    #[test_case("Need a token.\n**GOAL BLOCKED**", Next::Blocked ; "blocked")]
    fn stops_when_the_agent_says_so(reply: &str, expected: Next) {
        assert_eq!(Goal::new(OBJECTIVE).after_run(reply), expected);
    }

    #[test]
    fn nudges_until_the_cap() {
        let mut goal = Goal::new(OBJECTIVE);
        for _ in 1..MAX_RUNS {
            assert!(matches!(goal.after_run("still going"), Next::Continue(_)));
        }
        assert_eq!(goal.after_run("still going"), Next::OutOfRuns);
        assert_eq!(goal.runs, MAX_RUNS);
    }

    #[test]
    fn a_mention_mid_reply_is_not_a_verdict() {
        let mut goal = Goal::new(OBJECTIVE);
        assert!(matches!(
            goal.after_run("I will say GOAL ACHIEVED when done.\nRunning tests now."),
            Next::Continue(_)
        ));
    }
}
