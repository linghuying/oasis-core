package tendermint

import (
	"bytes"
	"context"

	"github.com/eapache/channels"
	"github.com/pkg/errors"
	tmtypes "github.com/tendermint/tendermint/types"

	beacon "github.com/oasislabs/ekiden/go/beacon/api"
	"github.com/oasislabs/ekiden/go/common/cbor"
	"github.com/oasislabs/ekiden/go/common/crypto/signature"
	"github.com/oasislabs/ekiden/go/common/logging"
	"github.com/oasislabs/ekiden/go/common/pubsub"
	epochtime "github.com/oasislabs/ekiden/go/epochtime/api"
	"github.com/oasislabs/ekiden/go/scheduler/api"
	registryapp "github.com/oasislabs/ekiden/go/tendermint/apps/registry"
	app "github.com/oasislabs/ekiden/go/tendermint/apps/scheduler"
	tmbeacon "github.com/oasislabs/ekiden/go/tendermint/componentapis/beacon"
	"github.com/oasislabs/ekiden/go/tendermint/service"
)

// BackendName is the name of this implementation.
const BackendName = "tendermint"

var (
	_ api.Backend      = (*tendermintScheduler)(nil)
	_ api.BlockBackend = (*tendermintScheduler)(nil)
)

type tendermintScheduler struct {
	logger *logging.Logger

	service  service.TendermintService
	notifier *pubsub.Broker
}

func (s *tendermintScheduler) Cleanup() {
}

func (s *tendermintScheduler) GetCommittees(ctx context.Context, id signature.PublicKey) ([]*api.Committee, error) {
	return s.GetBlockCommittees(ctx, id, 0)
}

func (s *tendermintScheduler) WatchCommittees() (<-chan *api.Committee, *pubsub.Subscription) {
	typedCh := make(chan *api.Committee)
	sub := s.notifier.Subscribe()
	sub.Unwrap(typedCh)

	return typedCh, sub
}

func (s *tendermintScheduler) GetBlockCommittees(ctx context.Context, id signature.PublicKey, height int64) ([]*api.Committee, error) {
	raw, err := s.service.Query(app.QueryAllCommittees, id, height)
	if err != nil {
		return nil, err
	}

	var committees []*api.Committee
	err = cbor.Unmarshal(raw, &committees)
	if err != nil {
		return nil, err
	}

	var runtimeCommittees []*api.Committee
	for _, c := range committees {
		if c.RuntimeID.Equal(id) {
			runtimeCommittees = append(runtimeCommittees, c)
		}
	}

	return runtimeCommittees, err
}

func (s *tendermintScheduler) getCurrentCommittees() ([]*api.Committee, error) {
	raw, err := s.service.Query(app.QueryAllCommittees, nil, 0)
	if err != nil {
		return nil, err
	}

	var committees []*api.Committee
	err = cbor.Unmarshal(raw, &committees)
	return committees, err
}

func (s *tendermintScheduler) worker(ctx context.Context) {
	// Subscribe to blocks which elect committees.
	sub, err := s.service.Subscribe("scheduler-worker", app.QueryElected)
	if err != nil {
		s.logger.Error("failed to subscribe",
			"err", err,
		)
		return
	}
	defer func() {
		err := s.service.Unsubscribe("scheduler-worker", app.QueryElected)
		if err != nil {
			s.logger.Error("failed to unsubscribe",
				"err", err,
			)
		}
	}()

	for {
		var event interface{}

		select {
		case msg := <-sub.Out():
			event = msg.Data()
		case <-sub.Cancelled():
			s.logger.Debug("worker: terminating, subscription closed")
			return
		case <-ctx.Done():
			return
		}

		switch ev := event.(type) {
		case tmtypes.EventDataNewBlock:
			s.onEventDataNewBlock(ctx, ev)
		default:
		}
	}
}

// Called from worker.
func (s *tendermintScheduler) onEventDataNewBlock(ctx context.Context, ev tmtypes.EventDataNewBlock) {
	tags := ev.ResultBeginBlock.GetTags()

	for _, pair := range tags {
		if bytes.Equal(pair.GetKey(), app.TagElected) {
			var kinds []api.CommitteeKind
			if err := cbor.Unmarshal(pair.GetValue(), &kinds); err != nil {
				s.logger.Error("worker: malformed elected committee types list",
					"err", err,
				)
				continue
			}

			raw, err := s.service.Query(app.QueryKindsCommittees, kinds, ev.Block.Header.Height)
			if err != nil {
				s.logger.Error("worker: couldn't query elected committees",
					"err", err,
				)
				continue
			}

			var committees []*api.Committee
			if err := cbor.Unmarshal(raw, &committees); err != nil {
				s.logger.Error("worker: malformed elected committees",
					"err", err,
				)
				continue
			}

			for _, c := range committees {
				s.notifier.Broadcast(c)
			}
		}
	}
}

// New constracts a new tendermint-based scheduler Backend instance.
func New(ctx context.Context,
	timeSource epochtime.Backend,
	beacon beacon.Backend,
	service service.TendermintService,
) (api.Backend, error) {
	// We can only work with a block-based epochtime.
	blockTimeSource, ok := timeSource.(epochtime.BlockBackend)
	if !ok {
		return nil, errors.New("scheduler/tendermint: need a block-based epochtime backend")
	}

	// We can only work with an ABCI beacon.
	abciBeacon, ok := beacon.(tmbeacon.Backend)
	if !ok {
		return nil, errors.New("scheduler/tendermint: need an ABCI beacon backend")
	}

	// Initialze and register the tendermint service component.
	app := app.New(blockTimeSource, abciBeacon)
	if err := service.RegisterApplication(app, []string{registryapp.AppName}); err != nil {
		return nil, err
	}

	s := &tendermintScheduler{
		logger:  logging.GetLogger("scheduler/tendermint"),
		service: service,
	}
	s.notifier = pubsub.NewBrokerEx(func(ch *channels.InfiniteChannel) {
		currentCommittees, err := s.getCurrentCommittees()
		if err != nil {
			s.logger.Error("couldn't get current committees. won't send them. good luck to the subscriber",
				"err", err,
			)
			return
		}
		for _, c := range currentCommittees {
			ch.In() <- c
		}
	})

	go s.worker(ctx)

	return s, nil
}
