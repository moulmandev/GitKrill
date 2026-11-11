//! What the Bitbucket, Gitea / Forgejo and Azure DevOps clients share: the
//! sign-in they use (looked for, refused, replaced by the next one found),
//! the rate limit, conditional requests, and one request below the API with
//! redirects followed only inside it.
//!
//! The GitHub and GitLab clients predate this and keep their own copies of
//! the same rules.

use super::*;

/// What a list request came back with.
pub(super) enum Got {
    /// A 2xx answer, to be read.
    Body(HttpResponse),
    /// Nothing changed since the last answer (304).
    Unchanged,
    /// The feature is off for this repository: an empty list, not an error.
    Nothing,
}

/// One repository's API, with the sign-in its requests are made with.
pub(super) struct Api {
    pub repo: RepoId,
    /// [`RepoId::api_base`]: every request goes below it.
    pub base: String,
    transport: Box<dyn Transport>,
    secrets: Box<dyn Secrets>,
    token: Option<(Token, TokenSource)>,
    /// Tokens the API refused.
    rejected: Vec<Token>,
    /// Whether `secrets` has been consulted since the last reason to.
    looked: bool,
    limiter: Limiter,
    /// Last `ETag` per list, for conditional requests.
    etags: Vec<Option<String>>,
    /// The token in use was accepted in this round of requests: a refusal
    /// now is about what it may do (a scope), not about the token itself.
    vouched: bool,
    /// Statuses that mean "this sign-in is refused": 401, and for Azure
    /// DevOps also 203 (its sign-in page, sent in place of a 401).
    refusals: &'static [u16],
    /// The service's own words in an error answer.
    message: fn(&str) -> String,
}

impl Api {
    pub fn new(
        repo: RepoId,
        transport: Box<dyn Transport>,
        secrets: Box<dyn Secrets>,
        lists: usize,
        refusals: &'static [u16],
        message: fn(&str) -> String,
    ) -> Self {
        Api {
            base: repo.api_base(),
            repo,
            transport,
            secrets,
            token: None,
            rejected: Vec::new(),
            looked: false,
            limiter: Limiter::default(),
            etags: vec![None; lists],
            vouched: false,
            refusals,
            message,
        }
    }

    /// "Bitbucket", "Codeberg", "Azure DevOps"...
    pub fn name(&self) -> &'static str {
        self.repo.service.name()
    }

    /// The service's words in `body`.
    pub fn message(&self, body: &str) -> String {
        (self.message)(body)
    }

    /// Replaces the identity requests are made with. Validators and rate
    /// limits belong to an identity, so both start over.
    fn sign_in(&mut self, token: Option<(Token, TokenSource)>) {
        if self.token.as_ref().map(|(token, _)| token) != token.as_ref().map(|(token, _)| token) {
            self.etags.iter_mut().for_each(|etag| *etag = None);
            self.limiter = Limiter::default();
            self.vouched = false;
        }
        self.token = token;
        self.looked = true;
    }

    /// Looks for a token now if there is reason to.
    fn look(&mut self) {
        if !self.looked {
            let found = find_token(self.secrets.as_ref(), &self.repo, &self.rejected);
            self.sign_in(found);
        }
    }

    /// The token in use was refused: remember that, and move on to the
    /// next source (ending with none at all).
    fn refuse_token(&mut self) {
        if let Some((token, _)) = self.token.take() {
            self.rejected.push(token);
        }
        let found = find_token(self.secrets.as_ref(), &self.repo, &self.rejected);
        self.sign_in(found);
    }

    pub fn signed_in(&self) -> bool {
        self.token.is_some()
    }

    /// Why an anonymous request was turned away: no sign-in at all, or
    /// every one found was refused.
    pub fn anonymous(&self) -> Problem {
        if self.rejected.is_empty() { Problem::NotSignedIn } else { Problem::SignInRejected }
    }

    /// `problem` with every token this client has held blanked out.
    pub fn scrub(&self, problem: Problem) -> Problem {
        let mut tokens: Vec<&Token> = self.rejected.iter().collect();
        tokens.extend(self.token.as_ref().map(|(token, _)| token));
        problem.scrubbed(&tokens)
    }

    pub fn auth(&self) -> Auth {
        match &self.token {
            Some((_, source)) => Auth::SignedIn(*source),
            None if !self.looked => Auth::Unknown,
            None if self.rejected.is_empty() => Auth::Anonymous,
            None => Auth::Rejected,
        }
    }

    /// The user asked for a refresh: they may have signed in since. A
    /// working token is kept.
    pub fn look_again(&mut self) {
        if self.token.is_none() {
            self.looked = false;
        }
    }

    pub fn blocked_until(&self, now: u64) -> Option<u64> {
        self.limiter.blocked(now)
    }

    /// A new round of requests starts: the token has to be accepted again
    /// before a refusal is read as a missing scope.
    pub fn start_round(&mut self) {
        self.vouched = false;
    }

    /// Do not let a later "not modified" vouch for an answer that could not
    /// be read.
    pub fn forget_etag(&mut self, slot: usize) {
        if let Some(etag) = self.etags.get_mut(slot) {
            *etag = None;
        }
    }

    fn unexpected_redirect(&self) -> Problem {
        Problem::Failed(format!("{} sent an unexpected redirect", self.name()))
    }

    /// What a refusal of an accepted token means.
    fn scope_problem(&self) -> Problem {
        Problem::Refused("this sign-in may not do this (does its token have the scope?)".to_owned())
    }

    /// Takes note of an answer's rate-limit headers; the time to wait
    /// until when it was a refusal for asking too often.
    fn limited(&mut self, response: &HttpResponse, now: u64) -> Option<u64> {
        let message = self.message(&response.body);
        self.limiter.record(response.status, &Rate::of(response), &message, now)
    }

    /// GET `url` (below the API), conditionally when `slot` names a list
    /// whose last answer had an `ETag`. Redirects are followed within the
    /// API only; a refused token gives way to the next one found. Returns
    /// the final answer, whatever its status, for the caller to read.
    pub fn get(&mut self, url: String, slot: Option<usize>, now: u64) -> Result<HttpResponse, Problem> {
        let mut url = url;
        let mut hops = 0;
        loop {
            self.look();
            if let Some(until) = self.limiter.blocked(now) {
                return Err(Problem::RateLimited { until });
            }
            let etag = slot.and_then(|slot| self.etags.get(slot).cloned().flatten());
            let request =
                HttpRequest { url: &url, etag: etag.as_deref(), token: self.token.as_ref().map(|(token, _)| token) };
            let response = self.transport.get(&request).map_err(Problem::Unreachable)?;
            if let Some(until) = self.limited(&response, now) {
                return Err(Problem::RateLimited { until });
            }
            match response.status {
                301 | 302 | 303 | 307 | 308 => match response.header("location") {
                    Some(location) if within(&self.base, location) && hops < 3 => {
                        url = location.to_owned();
                        hops += 1;
                        continue;
                    }
                    _ => return Err(self.unexpected_redirect()),
                },
                status if self.refusals.contains(&status) && self.token.is_some() => {
                    if self.vouched {
                        return Err(self.scope_problem());
                    }
                    self.refuse_token();
                    continue;
                }
                status => {
                    if (200..300).contains(&status) || status == 304 {
                        self.vouched |= self.token.is_some();
                    }
                    if let (Some(slot), 200..=299) = (slot, status)
                        && let Some(etag) = self.etags.get_mut(slot)
                    {
                        *etag = response.header("etag").map(str::to_owned);
                    }
                    return Ok(response);
                }
            }
        }
    }

    /// POST `json` to `url` (below the API). With `signed`, nothing is sent
    /// without a token (opening a pull request anonymously is impossible).
    /// Never repeated after a request that may have reached the service,
    /// except after a refusal of the token, which means nothing was done;
    /// never redirected. Returns the answer, whatever its status.
    pub fn post(&mut self, url: &str, json: &str, signed: bool, now: u64) -> Result<HttpResponse, Problem> {
        loop {
            self.look();
            if signed && self.token.is_none() {
                return Err(self.anonymous());
            }
            if let Some(until) = self.limiter.blocked(now) {
                return Err(Problem::RateLimited { until });
            }
            let request = HttpRequest { url, etag: None, token: self.token.as_ref().map(|(token, _)| token) };
            let response = self.transport.post(&request, json).map_err(Problem::Unreachable)?;
            if let Some(until) = self.limited(&response, now) {
                return Err(Problem::RateLimited { until });
            }
            match response.status {
                300..=399 => return Err(self.unexpected_redirect()),
                status if self.refusals.contains(&status) && self.token.is_some() => {
                    if self.vouched {
                        return Err(self.scope_problem());
                    }
                    self.refuse_token();
                    continue;
                }
                status => {
                    if (200..300).contains(&status) {
                        self.vouched |= self.token.is_some();
                    }
                    return Ok(response);
                }
            }
        }
    }

    /// The usual reading of a list answer: 2xx is the list, 304 unchanged;
    /// without a token, a refusal or "not found" means the repository is
    /// private; with one, "not found" means no access, and a refusal is
    /// shown in the service's words.
    pub fn classify(&self, response: HttpResponse) -> Result<Got, Problem> {
        let message = self.message(&response.body);
        match response.status {
            200..=299 => Ok(Got::Body(response)),
            304 => Ok(Got::Unchanged),
            status if !self.signed_in() && (matches!(status, 401 | 403 | 404) || self.refusals.contains(&status)) => {
                Err(self.anonymous())
            }
            404 => Err(Problem::NoAccess),
            403 if !message.is_empty() => Err(Problem::Refused(message)),
            status => Err(Problem::Failed(format!("{} answered {status}", self.name()))),
        }
    }

    /// One list: GET `url`, let `special` read the statuses the service
    /// gives a meaning of its own (it is told whether a token was used),
    /// [`Api::classify`] the rest, and `parse` the body into the items and
    /// whether the service has more. Problems leave with tokens scrubbed.
    pub fn fetch<T>(
        &mut self,
        url: String,
        slot: usize,
        now: u64,
        special: impl FnOnce(&HttpResponse, bool) -> Option<Result<Got, Problem>>,
        parse: impl FnOnce(&HttpResponse) -> Result<(Vec<T>, bool), String>,
    ) -> Outcome<T> {
        let got = self.get(url, Some(slot), now).and_then(|response| {
            match special(&response, self.signed_in()) {
                Some(answer) => answer,
                None => self.classify(response),
            }
        });
        let outcome = match got {
            Ok(Got::Unchanged) => Outcome::Unchanged,
            Ok(Got::Nothing) => Outcome::Fresh { items: Vec::new(), more: false },
            Ok(Got::Body(response)) => match parse(&response) {
                Ok((items, more)) => Outcome::Fresh { items, more },
                Err(error) => {
                    self.forget_etag(slot);
                    Outcome::Failed(self.unreadable(error))
                }
            },
            Err(problem) => Outcome::Failed(problem),
        };
        match outcome {
            Outcome::Failed(problem) => Outcome::Failed(self.scrub(problem)),
            fine => fine,
        }
    }

    pub fn unreadable(&self, error: String) -> Problem {
        Problem::Failed(format!("Unreadable answer from {} ({error})", self.name()))
    }
}

/// `serde_json` errors in a few words: what was expected and where.
pub(super) fn json<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, String> {
    serde_json::from_str(text).map_err(|e| e.to_string())
}

/// What an error body says when it is `{"message": "..."}`, or nothing.
pub(super) fn plain_message(body: &str) -> String {
    #[derive(Deserialize)]
    struct RawError {
        message: String,
    }
    serde_json::from_str::<RawError>(body).map(|error| error.message).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::super::tests::{Fake, Log, NOW, OTHER, SECRET, Script, reply};
    use super::*;

    fn scripted(replies: Vec<Result<HttpResponse, String>>, secrets: Fake) -> (Api, Log) {
        let sent = Log::default();
        let transport = Script { replies: replies.into(), sent: sent.clone() };
        let repo = RepoId {
            service: Service::Codeberg,
            host: "codeberg.org".to_owned(),
            owner: "team".to_owned(),
            name: "app".to_owned(),
        };
        (Api::new(repo, Box::new(transport), Box::new(secrets), 3, &[401], plain_message), sent)
    }

    fn two_tokens() -> Fake {
        Fake { env: vec![("CODEBERG_TOKEN", SECRET)], cli: Some(OTHER), ..Default::default() }
    }

    #[test]
    fn a_refused_token_gives_way_to_the_next_until_one_is_accepted() {
        let (mut api, sent) = scripted(vec![reply(401, &[], "{}"), reply(200, &[("etag", "\"a\"")], "[]")], two_tokens());
        let response = api.get(format!("{}/repos/team/app/pulls", api.base), Some(0), NOW).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(api.auth(), Auth::SignedIn(TokenSource::Cli));
        let tokens: Vec<Option<String>> = sent.lock().unwrap().iter().map(|request| request.token.clone()).collect();
        assert_eq!(tokens, [Some(SECRET.to_owned()), Some(OTHER.to_owned())]);
    }

    #[test]
    fn a_token_accepted_this_round_is_kept_when_another_list_refuses_it() {
        let (mut api, sent) = scripted(
            vec![reply(200, &[], "[]"), reply(401, &[], "{}"), reply(401, &[], "{}"), reply(200, &[], "[]")],
            two_tokens(),
        );
        let base = api.base.clone();
        assert_eq!(api.get(format!("{base}/a"), Some(0), NOW).unwrap().status, 200);
        // Accepted a moment ago: the refusal is about a scope.
        assert!(matches!(api.get(format!("{base}/b"), Some(1), NOW), Err(Problem::Refused(_))));
        assert_eq!(api.auth(), Auth::SignedIn(TokenSource::Env("CODEBERG_TOKEN")));
        // A new round: the same refusal now means the token is gone.
        api.start_round();
        assert_eq!(api.get(format!("{base}/a"), Some(0), NOW).unwrap().status, 200);
        assert_eq!(api.auth(), Auth::SignedIn(TokenSource::Cli));
        assert_eq!(sent.lock().unwrap().len(), 4);
    }

    #[test]
    fn signed_posts_are_never_sent_without_a_token_nor_redirected() {
        let (mut api, sent) = scripted(vec![reply(201, &[], "{}")], Fake::default());
        let url = format!("{}/repos/team/app/pulls", api.base);
        assert_eq!(api.post(&url, "{}", true, NOW).unwrap_err(), Problem::NotSignedIn);
        assert!(sent.lock().unwrap().is_empty());

        let moved = [("location", "https://codeberg.org/api/v1/repos/team/other/pulls")];
        let (mut api, sent) = scripted(vec![reply(307, &moved, ""), reply(201, &[], "{}")], two_tokens());
        assert!(matches!(api.post(&url, "{}", true, NOW), Err(Problem::Failed(text)) if text.contains("redirect")));
        assert_eq!(sent.lock().unwrap().len(), 1);
    }

    #[test]
    fn statuses_read_as_the_usual_problems() {
        let (api, _) = scripted(Vec::new(), Fake::default());
        let status = |status: u16, body: &str| api.classify(HttpResponse { status, body: body.to_owned(), ..Default::default() });
        assert!(matches!(status(200, "[]"), Ok(Got::Body(_))));
        assert!(matches!(status(304, ""), Ok(Got::Unchanged)));
        for refused in [401, 403, 404] {
            assert_eq!(status(refused, "{}").err(), Some(Problem::NotSignedIn), "{refused}");
        }
        assert_eq!(status(502, "<html>").err(), Some(Problem::Failed("Codeberg answered 502".to_owned())));
    }
}
