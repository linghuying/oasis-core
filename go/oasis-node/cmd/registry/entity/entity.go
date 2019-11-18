// Package entity implements the entity registry sub-commands.
package entity

import (
	"context"
	"encoding/json"
	"fmt"
	"io/ioutil"
	"os"
	"path/filepath"
	"time"

	"github.com/spf13/cobra"
	flag "github.com/spf13/pflag"
	"github.com/spf13/viper"
	"google.golang.org/grpc"

	"github.com/oasislabs/oasis-core/go/common/crypto/signature"
	fileSigner "github.com/oasislabs/oasis-core/go/common/crypto/signature/signers/file"
	"github.com/oasislabs/oasis-core/go/common/entity"
	"github.com/oasislabs/oasis-core/go/common/logging"
	"github.com/oasislabs/oasis-core/go/common/node"
	grpcRegistry "github.com/oasislabs/oasis-core/go/grpc/registry"
	cmdCommon "github.com/oasislabs/oasis-core/go/oasis-node/cmd/common"
	cmdFlags "github.com/oasislabs/oasis-core/go/oasis-node/cmd/common/flags"
	cmdGrpc "github.com/oasislabs/oasis-core/go/oasis-node/cmd/common/grpc"
	registry "github.com/oasislabs/oasis-core/go/registry/api"
)

const (
	cfgAllowEntitySignedNodes = "entity.debug.allow_entity_signed_nodes"
	cfgNodeID                 = "entity.node.id"
	cfgNodeDescriptor         = "entity.node.descriptor"
	cfgTxFile                 = "entity.transaction.file"

	entityGenesisFilename = "entity_genesis.json"
)

var (
	entityFlags               = flag.NewFlagSet("", flag.ContinueOnError)
	initFlags                 = flag.NewFlagSet("", flag.ContinueOnError)
	updateFlags               = flag.NewFlagSet("", flag.ContinueOnError)
	registerOrDeregisterFlags = flag.NewFlagSet("", flag.ContinueOnError)
	submitFlags               = flag.NewFlagSet("", flag.ContinueOnError)
	txFileFlags               = flag.NewFlagSet("", flag.ContinueOnError)

	entityCmd = &cobra.Command{
		Use:   "entity",
		Short: "entity registry backend utilities",
	}

	initCmd = &cobra.Command{
		Use:   "init",
		Short: "initialize an entity",
		Run:   doInit,
	}

	updateCmd = &cobra.Command{
		Use:   "update",
		Short: "update an entity",
		Run:   doUpdate,
	}

	registerCmd = &cobra.Command{
		Use:   "gen_register",
		Short: "generate a register entity transaction",
		Run:   doGenRegister,
	}

	deregisterCmd = &cobra.Command{
		Use:   "gen_deregister",
		Short: "generate a deregister entity transaction",
		Run:   doGenDeregister,
	}

	listCmd = &cobra.Command{
		Use:   "list",
		Short: "list registered entities",
		Run:   doList,
	}

	submitCmd = &cobra.Command{
		Use:   "submit",
		Short: "submit a pre-generated transaction",
		Run:   doSubmit,
	}

	logger = logging.GetLogger("cmd/registry/entity")
)

type serializedTx struct {
	Register   *entity.SignedEntity `json:"register,omitempty"`
	Deregister *signature.Signed    `json:"deregister,omitempty"`
}

type generateEntitySignedTxFunc func(txFilePath string, ent *entity.Entity, signer signature.Signer) (*serializedTx, error)

func doConnect(cmd *cobra.Command) (*grpc.ClientConn, grpcRegistry.EntityRegistryClient) {
	conn, err := cmdGrpc.NewClient(cmd)
	if err != nil {
		logger.Error("failed to establish connection with node",
			"err", err,
		)
		os.Exit(1)
	}

	client := grpcRegistry.NewEntityRegistryClient(conn)

	return conn, client
}

func doInit(cmd *cobra.Command, args []string) {
	if err := cmdCommon.Init(); err != nil {
		cmdCommon.EarlyLogAndExit(err)
	}

	dataDir, err := cmdCommon.DataDirOrPwd()
	if err != nil {
		logger.Error("failed to query data directory",
			"err", err,
		)
		os.Exit(1)
	}

	// Loosely check to see if there is an existing entity.  This isn't
	// perfect, just "oopsie" avoidance.
	if _, _, err = loadOrGenerateEntity(dataDir, false); err == nil {
		switch cmdFlags.Force() {
		case true:
			logger.Warn("overwriting existing entity")
		default:
			logger.Error("existing entity exists, specifiy --force to overwrite")
			os.Exit(1)
		}
	}

	// Generate a new entity.
	ent, signer, err := loadOrGenerateEntity(dataDir, true)
	if err != nil {
		logger.Error("failed to generate entity",
			"err", err,
		)
		os.Exit(1)
	}

	if err = signAndWriteEntityGenesis(dataDir, signer, ent); err != nil {
		os.Exit(1)
	}

	logger.Info("generated entity",
		"entity", ent.ID,
	)
}

func doUpdate(cmd *cobra.Command, args []string) {
	if err := cmdCommon.Init(); err != nil {
		cmdCommon.EarlyLogAndExit(err)
	}

	dataDir, err := cmdCommon.DataDirOrPwd()
	if err != nil {
		logger.Error("failed to query data directory",
			"err", err,
		)
		os.Exit(1)
	}

	// Load the existing entity.
	ent, signer, err := loadOrGenerateEntity(dataDir, false)
	if err != nil {
		logger.Error("failed to load entity",
			"err", err,
		)
		os.Exit(1)
	}

	// Update the entity.
	ent.RegistrationTime = uint64(time.Now().Unix())
	ent.AllowEntitySignedNodes = viper.GetBool(cfgAllowEntitySignedNodes)

	ent.Nodes = nil
	for _, v := range viper.GetStringSlice(cfgNodeID) {
		var nodeID signature.PublicKey
		if err = nodeID.UnmarshalHex(v); err != nil {
			logger.Error("failed to parse node ID",
				"err", err,
				"node_id", v,
			)
			os.Exit(1)
		}
		ent.Nodes = append(ent.Nodes, nodeID)
	}
	for _, v := range viper.GetStringSlice(cfgNodeDescriptor) {
		var b []byte
		if b, err = ioutil.ReadFile(v); err != nil {
			logger.Error("failed to read node descriptor",
				"err", err,
				"path", v,
			)
			os.Exit(1)
		}

		var signedNode node.SignedNode
		if err = json.Unmarshal(b, &signedNode); err != nil {
			logger.Error("failed to parse signed node descriptor",
				"err", err,
			)
			os.Exit(1)
		}

		var n node.Node
		if err = signedNode.Open(registry.RegisterGenesisNodeSignatureContext, &n); err != nil {
			logger.Error("failed to validate signed node descriptor",
				"err", err,
			)
			os.Exit(1)
		}

		if !ent.ID.Equal(n.EntityID) {
			logger.Error("entity ID mismatch, node does not belong to this entity",
				"entity_id", ent.ID,
				"node_entity_id", n.EntityID,
			)
			os.Exit(1)
		}
		if !signedNode.Signature.PublicKey.Equal(n.ID) {
			logger.Error("node genesis descriptor is not self signed",
				"signer", signedNode.Signature.PublicKey,
			)
			os.Exit(1)
		}
		ent.Nodes = append(ent.Nodes, n.ID)
	}

	// De-duplicate the entity's nodes.
	nodeMap := make(map[signature.PublicKey]bool)
	for _, v := range ent.Nodes {
		nodeMap[v] = true
	}
	ent.Nodes = make([]signature.PublicKey, 0, len(nodeMap))
	for k := range nodeMap {
		ent.Nodes = append(ent.Nodes, k)
	}

	// Save the entity descriptor.
	if err = ent.Save(dataDir); err != nil {
		logger.Error("failed to persist entity descriptor",
			"err", err,
		)
		os.Exit(1)
	}

	// Regenerate the genesis document entity registration.
	if err = signAndWriteEntityGenesis(dataDir, signer, ent); err != nil {
		os.Exit(1)
	}

	logger.Info("updated entity",
		"entity", ent.ID,
	)
}

func signAndWriteEntityGenesis(dataDir string, signer signature.Signer, ent *entity.Entity) error {
	// Sign the entity registration for use in a genesis document.
	signed, err := entity.SignEntity(signer, registry.RegisterGenesisEntitySignatureContext, ent)
	if err != nil {
		logger.Error("failed to sign entity for genesis registration",
			"err", err,
		)
		return err
	}

	// Write out the signed entity registration.
	b, _ := json.Marshal(signed)
	if err = ioutil.WriteFile(filepath.Join(dataDir, entityGenesisFilename), b, 0600); err != nil {
		logger.Error("failed to write signed entity genesis registration",
			"err", err,
		)
		return err
	}

	return nil
}

func saveSignedTx(tx *serializedTx, txFilePath string) error {
	b, _ := json.Marshal(tx)
	if err := ioutil.WriteFile(txFilePath, b, 0600); err != nil {
		return err
	}
	return nil
}

func loadSignedTx(txFilePath string) (*serializedTx, error) {
	var tx serializedTx
	b, err := ioutil.ReadFile(txFilePath)
	if err != nil {
		return nil, err
	}
	if err = json.Unmarshal(b, &tx); err != nil {
		return nil, err
	}
	return &tx, nil
}

func doGenerateEntitySignedTx(signedTxFunc generateEntitySignedTxFunc, cmd *cobra.Command, args []string) {
	if err := cmdCommon.Init(); err != nil {
		cmdCommon.EarlyLogAndExit(err)
	}

	txFilePath := viper.GetString(cfgTxFile)
	if txFilePath == "" {
		logger.Error("must specify an --" + cfgTxFile)
		os.Exit(1)
	}

	dataDir, err := cmdCommon.DataDirOrPwd()
	if err != nil {
		logger.Error("failed to query data directory",
			"err", err,
		)
		os.Exit(1)
	}

	ent, signer, err := loadOrGenerateEntity(dataDir, false)
	if err != nil {
		logger.Error("failed to load entity",
			"err", err,
		)
		os.Exit(1)
	}

	tx, err := signedTxFunc(txFilePath, ent, signer)
	if err != nil {
		logger.Error("failed to sign transaction",
			"err", err,
		)
		os.Exit(1)
	}

	if err = saveSignedTx(tx, txFilePath); err != nil {
		logger.Error("failed to save signed transaction",
			"err", err,
		)
		os.Exit(1)
	}
}

func doGenRegister(cmd *cobra.Command, args []string) {
	doGenerateEntitySignedTx(doSignRegister, cmd, args)
}

func doSignRegister(txFilePath string, ent *entity.Entity, signer signature.Signer) (*serializedTx, error) {
	ent.RegistrationTime = uint64(time.Now().Unix())
	signed, err := entity.SignEntity(signer, registry.RegisterEntitySignatureContext, ent)
	if err != nil {
		return nil, err
	}

	tx := &serializedTx{
		Register: signed,
	}
	return tx, nil
}

func doGenDeregister(cmd *cobra.Command, args []string) {
	doGenerateEntitySignedTx(doSignDeregister, cmd, args)
}

func doSignDeregister(txFilePath string, ent *entity.Entity, signer signature.Signer) (*serializedTx, error) {
	ts := registry.Timestamp(time.Now().Unix())
	signed, err := signature.SignSigned(signer, registry.DeregisterEntitySignatureContext, &ts)
	if err != nil {
		return nil, err
	}

	tx := &serializedTx{
		Deregister: signed,
	}
	return tx, nil
}

func doSubmit(cmd *cobra.Command, args []string) {
	txFilePath := viper.GetString(cfgTxFile)
	if txFilePath == "" {
		logger.Error("must specify an --" + cfgTxFile)
		os.Exit(1)
	}
	tx, err := loadSignedTx(txFilePath)
	if err != nil {
		logger.Error("failed to load signed transaction",
			"err", err,
		)
		os.Exit(1)
	}

	nrRetries := cmdFlags.Retries()
	for i := 0; i <= nrRetries; {
		if err = func() error {
			conn, client := doConnect(cmd)
			defer conn.Close()

			var actErr error
			if tx.Register != nil {
				actErr = doSubmitRegister(client, tx.Register)
			} else if tx.Deregister != nil {
				actErr = doSubmitDeregister(client, tx.Deregister)
			}
			return actErr
		}(); err == nil {
			return
		}

		if nrRetries > 0 {
			i++
		}
		if i <= nrRetries {
			time.Sleep(1 * time.Second)
		}
	}
	os.Exit(1)
}

func doSubmitRegister(client grpcRegistry.EntityRegistryClient, signed *entity.SignedEntity) error {
	req := &grpcRegistry.RegisterRequest{
		Entity: signed.ToProto(),
	}
	if _, err := client.RegisterEntity(context.Background(), req); err != nil {
		logger.Error("failed to register entity",
			"err", err,
		)
		return err
	}

	logger.Info("registered entity",
		"entity", signed.Signature.PublicKey,
	)

	return nil
}

func doSubmitDeregister(client grpcRegistry.EntityRegistryClient, signed *signature.Signed) error {
	req := &grpcRegistry.DeregisterRequest{
		Timestamp: signed.ToProto(),
	}
	if _, err := client.DeregisterEntity(context.Background(), req); err != nil {
		logger.Error("failed to deregister entity",
			"err", err,
		)
		return err
	}

	logger.Info("deregistered entity",
		"entity", signed.Signature.PublicKey,
	)
	return nil
}

func doList(cmd *cobra.Command, args []string) {
	if err := cmdCommon.Init(); err != nil {
		cmdCommon.EarlyLogAndExit(err)
	}

	conn, client := doConnect(cmd)
	defer conn.Close()

	entities, err := client.GetEntities(context.Background(), &grpcRegistry.EntitiesRequest{})
	if err != nil {
		logger.Error("failed to query entities",
			"err", err,
		)
		os.Exit(1)
	}

	for _, v := range entities.GetEntity() {
		var ent entity.Entity
		if err = ent.FromProto(v); err != nil {
			logger.Error("failed to de-serialize entity",
				"err", err,
				"pb", v,
			)
			continue
		}

		var s string
		switch cmdFlags.Verbose() {
		case true:
			b, _ := json.Marshal(&ent)
			s = string(b)
		default:
			s = ent.ID.String()
		}

		fmt.Printf("%v\n", s)
	}
}

func loadOrGenerateEntity(dataDir string, generate bool) (*entity.Entity, signature.Signer, error) {
	if cmdFlags.DebugTestEntity() {
		return entity.TestEntity()
	}

	// TODO/hsm: Configure factory dynamically.
	entitySignerFactory := fileSigner.NewFactory(dataDir, signature.SignerEntity)
	if generate {
		template := &entity.Entity{
			AllowEntitySignedNodes: viper.GetBool(cfgAllowEntitySignedNodes),
		}

		return entity.Generate(dataDir, entitySignerFactory, template)
	}

	return entity.Load(dataDir, entitySignerFactory)
}

// Register registers the entity sub-command and all of it's children.
func Register(parentCmd *cobra.Command) {
	for _, v := range []*cobra.Command{
		initCmd,
		updateCmd,
		registerCmd,
		deregisterCmd,
		listCmd,
		submitCmd,
	} {
		entityCmd.AddCommand(v)
	}

	initCmd.Flags().AddFlagSet(initFlags)
	updateCmd.Flags().AddFlagSet(updateFlags)
	registerCmd.Flags().AddFlagSet(registerOrDeregisterFlags)
	deregisterCmd.Flags().AddFlagSet(registerOrDeregisterFlags)
	submitCmd.Flags().AddFlagSet(submitFlags)

	listCmd.Flags().AddFlagSet(cmdFlags.VerboseFlags)
	listCmd.Flags().AddFlagSet(cmdGrpc.ClientFlags)

	parentCmd.AddCommand(entityCmd)
}

func init() {
	entityFlags.Bool(cfgAllowEntitySignedNodes, false, "Entity signing key may be used for node registration (UNSAFE)")
	_ = viper.BindPFlags(entityFlags)

	initFlags.AddFlagSet(cmdFlags.ForceFlags)
	initFlags.AddFlagSet(cmdFlags.DebugTestEntityFlags)
	initFlags.AddFlagSet(entityFlags)

	updateFlags.StringSlice(cfgNodeID, nil, "ID(s) of nodes associated with this entity")
	updateFlags.StringSlice(cfgNodeDescriptor, nil, "Node genesis descriptor(s) of nodes associated with this entity")
	_ = viper.BindPFlags(updateFlags)
	updateFlags.AddFlagSet(cmdFlags.DebugTestEntityFlags)
	updateFlags.AddFlagSet(entityFlags)

	txFileFlags.String(cfgTxFile, "", "Signed transaction file path")
	_ = viper.BindPFlags(txFileFlags)

	registerOrDeregisterFlags.AddFlagSet(cmdFlags.RetriesFlags)
	registerOrDeregisterFlags.AddFlagSet(cmdFlags.DebugTestEntityFlags)
	registerOrDeregisterFlags.AddFlagSet(cmdGrpc.ClientFlags)
	registerOrDeregisterFlags.AddFlagSet(txFileFlags)

	submitFlags.AddFlagSet(cmdFlags.RetriesFlags)
	submitFlags.AddFlagSet(cmdGrpc.ClientFlags)
	submitFlags.AddFlagSet(txFileFlags)
}
